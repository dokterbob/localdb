use super::*;

// ---------------------------------------------------------------------
// Grants
// ---------------------------------------------------------------------

#[tokio::test]
async fn grant_store_then_list_for_user_and_store() {
    let (_dir, backend, store) = make_store().await;
    insert_store_row(&backend, "docs").await;
    store
        .create_user(&make_user("u1", "bob", Role::Member))
        .await
        .unwrap();
    let grant = StoreGrantRow {
        store_name: "docs".to_string(),
        user_id: "u1".to_string(),
        granted_by: "admin-1".to_string(),
        created_at: "2026-06-10T12:00:00Z".to_string(),
    };
    store.grant_store(&grant).await.unwrap();

    let for_user = store.list_grants_for_user("u1").await.unwrap();
    assert_eq!(for_user, vec![grant.clone()]);

    let for_store = store.list_grants_for_store("docs").await.unwrap();
    assert_eq!(for_store, vec![grant]);
}

#[tokio::test]
async fn grant_store_upserts_on_same_store_user_pair() {
    let (_dir, backend, store) = make_store().await;
    insert_store_row(&backend, "docs").await;
    store
        .create_user(&make_user("u1", "bob", Role::Member))
        .await
        .unwrap();
    store
        .grant_store(&StoreGrantRow {
            store_name: "docs".to_string(),
            user_id: "u1".to_string(),
            granted_by: "admin-1".to_string(),
            created_at: "2026-06-10T12:00:00Z".to_string(),
        })
        .await
        .unwrap();
    // Re-grant with a different `granted_by` — must update, not duplicate.
    store
        .grant_store(&StoreGrantRow {
            store_name: "docs".to_string(),
            user_id: "u1".to_string(),
            granted_by: "admin-2".to_string(),
            created_at: "2026-06-11T12:00:00Z".to_string(),
        })
        .await
        .unwrap();

    let grants = store.list_grants_for_user("u1").await.unwrap();
    assert_eq!(grants.len(), 1, "re-granting must not duplicate the row");
    assert_eq!(grants[0].granted_by, "admin-2");
}

#[tokio::test]
async fn revoke_store_grant_removes_it_and_reports_result() {
    let (_dir, backend, store) = make_store().await;
    insert_store_row(&backend, "docs").await;
    store
        .create_user(&make_user("u1", "bob", Role::Member))
        .await
        .unwrap();
    store
        .grant_store(&StoreGrantRow {
            store_name: "docs".to_string(),
            user_id: "u1".to_string(),
            granted_by: "admin-1".to_string(),
            created_at: "2026-06-10T12:00:00Z".to_string(),
        })
        .await
        .unwrap();

    assert!(store.revoke_store_grant("docs", "u1").await.unwrap());
    assert!(store.list_grants_for_user("u1").await.unwrap().is_empty());
    assert!(!store.revoke_store_grant("docs", "u1").await.unwrap());
}

#[tokio::test]
async fn store_grant_cascades_on_user_delete() {
    let (_dir, backend, store) = make_store().await;
    insert_store_row(&backend, "docs").await;
    store
        .create_user(&make_user("u1", "bob", Role::Member))
        .await
        .unwrap();
    store
        .grant_store(&StoreGrantRow {
            store_name: "docs".to_string(),
            user_id: "u1".to_string(),
            granted_by: "admin-1".to_string(),
            created_at: "2026-06-10T12:00:00Z".to_string(),
        })
        .await
        .unwrap();

    store.delete_user("u1").await.unwrap();

    assert!(
        store
            .list_grants_for_store("docs")
            .await
            .unwrap()
            .is_empty(),
        "grants must cascade-delete when their user is removed"
    );
}
