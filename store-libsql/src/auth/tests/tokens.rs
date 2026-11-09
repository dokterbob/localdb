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
async fn revoke_compromised_family_hits_family_members_and_tainted_rows_only() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();

    let mut refresh = make_token("t1", "u1", TokenKind::Refresh, "hash-a");
    refresh.family_id = Some("fam-1".to_string());
    let mut access = make_token("t2", "u1", TokenKind::Access, "hash-b");
    access.family_id = Some("fam-1".to_string());
    let mut tainted_key = make_token("t3", "u1", TokenKind::ApiKey, "hash-c");
    tainted_key.minted_by_family = Some("fam-1".to_string());
    let mut other_family = make_token("t4", "u1", TokenKind::Refresh, "hash-d");
    other_family.family_id = Some("fam-2".to_string());
    let untainted_key = make_token("t5", "u1", TokenKind::ApiKey, "hash-e");

    store.insert_token(&refresh).await.unwrap();
    store.insert_token(&access).await.unwrap();
    store.insert_token(&tainted_key).await.unwrap();
    store.insert_token(&other_family).await.unwrap();
    store.insert_token(&untainted_key).await.unwrap();

    let revoked_count = store.revoke_compromised_family("fam-1").await.unwrap();
    assert_eq!(revoked_count, 3, "refresh + access + the tainted key");

    for hash in ["hash-a", "hash-b", "hash-c"] {
        assert!(
            store
                .find_token_by_hash(hash)
                .await
                .unwrap()
                .unwrap()
                .revoked_at
                .is_some(),
            "{hash} must be revoked"
        );
    }
    for hash in ["hash-d", "hash-e"] {
        assert!(
            store
                .find_token_by_hash(hash)
                .await
                .unwrap()
                .unwrap()
                .revoked_at
                .is_none(),
            "{hash} must be left alone"
        );
    }
}

#[tokio::test]
async fn revoke_compromised_family_revokes_multi_level_descendants_and_nothing_else() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();

    // fam-1 (the root), fam-2 (minted by fam-1 — a session created by
    // authorizing with fam-1's own credential), and a key minted under
    // fam-2 — a two-level chain. A sibling family and an unrelated key must
    // be left alone.
    let mut root_refresh = make_token("t1", "u1", TokenKind::Refresh, "hash-a");
    root_refresh.family_id = Some("fam-1".to_string());
    let mut child_access = make_token("t2", "u1", TokenKind::Access, "hash-b");
    child_access.family_id = Some("fam-2".to_string());
    child_access.minted_by_family = Some("fam-1".to_string());
    let mut child_refresh = make_token("t3", "u1", TokenKind::Refresh, "hash-c");
    child_refresh.family_id = Some("fam-2".to_string());
    child_refresh.minted_by_family = Some("fam-1".to_string());
    let mut grandchild_key = make_token("t4", "u1", TokenKind::ApiKey, "hash-d");
    grandchild_key.minted_by_family = Some("fam-2".to_string());
    let mut sibling = make_token("t5", "u1", TokenKind::Refresh, "hash-e");
    sibling.family_id = Some("fam-3".to_string());
    let untainted_key = make_token("t6", "u1", TokenKind::ApiKey, "hash-f");

    for row in [
        &root_refresh,
        &child_access,
        &child_refresh,
        &grandchild_key,
        &sibling,
        &untainted_key,
    ] {
        store.insert_token(row).await.unwrap();
    }

    let revoked_count = store.revoke_compromised_family("fam-1").await.unwrap();
    assert_eq!(
        revoked_count, 4,
        "the root refresh token, both fam-2 rows, and the grandchild key"
    );

    for hash in ["hash-a", "hash-b", "hash-c", "hash-d"] {
        assert!(
            store
                .find_token_by_hash(hash)
                .await
                .unwrap()
                .unwrap()
                .revoked_at
                .is_some(),
            "{hash} is part of the closure and must be revoked"
        );
    }
    for hash in ["hash-e", "hash-f"] {
        assert!(
            store
                .find_token_by_hash(hash)
                .await
                .unwrap()
                .unwrap()
                .revoked_at
                .is_none(),
            "{hash} is outside the closure and must be left alone"
        );
    }
}

#[tokio::test]
async fn revoke_compromised_family_refuses_a_later_insert_under_the_same_family() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();
    let mut refresh = make_token("t1", "u1", TokenKind::Refresh, "hash-a");
    refresh.family_id = Some("fam-1".to_string());
    store.insert_token(&refresh).await.unwrap();

    store.revoke_compromised_family("fam-1").await.unwrap();

    let mut late_key = make_token("t2", "u1", TokenKind::ApiKey, "hash-late");
    late_key.minted_by_family = Some("fam-1".to_string());
    let err = store.insert_token(&late_key).await.unwrap_err();
    assert!(matches!(err, localdb_core::Error::Unauthorized { .. }));
    assert!(
        store
            .find_token_by_hash("hash-late")
            .await
            .unwrap()
            .is_none(),
        "the refused insert must not have persisted a row"
    );

    // An unrelated family is unaffected.
    let mut other_key = make_token("t3", "u1", TokenKind::ApiKey, "hash-other");
    other_key.minted_by_family = Some("fam-2".to_string());
    store.insert_token(&other_key).await.unwrap();
    assert!(store
        .find_token_by_hash("hash-other")
        .await
        .unwrap()
        .is_some());
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
