use super::*;

// ---------------------------------------------------------------------
// Users
// ---------------------------------------------------------------------

#[tokio::test]
async fn create_and_get_user_round_trips() {
    let (_dir, _backend, store) = make_store().await;
    let user = make_user("u1", "alice", Role::Admin);
    store.create_user(&user).await.unwrap();

    let found = store.get_user("u1").await.unwrap().unwrap();
    assert_eq!(found, user);

    let found_by_name = store.get_user_by_name("alice").await.unwrap().unwrap();
    assert_eq!(found_by_name, user);
}

#[tokio::test]
async fn get_user_missing_returns_none() {
    let (_dir, _backend, store) = make_store().await;
    assert!(store.get_user("nope").await.unwrap().is_none());
    assert!(store.get_user_by_name("nope").await.unwrap().is_none());
}

#[tokio::test]
async fn create_user_rejects_duplicate_name() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();
    let err = store
        .create_user(&make_user("u2", "alice", Role::Member))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));
}

#[tokio::test]
async fn list_users_returns_all() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();
    store
        .create_user(&make_user("u2", "bob", Role::Member))
        .await
        .unwrap();
    let all = store.list_users().await.unwrap();
    assert_eq!(all.len(), 2);
}

#[tokio::test]
async fn update_user_role_changes_role() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Member))
        .await
        .unwrap();
    store.update_user_role("u1", Role::Admin).await.unwrap();
    let found = store.get_user("u1").await.unwrap().unwrap();
    assert_eq!(found.role, Role::Admin);
}

#[tokio::test]
async fn update_user_role_errors_when_user_missing() {
    let (_dir, _backend, store) = make_store().await;
    let err = store
        .update_user_role("nope", Role::Admin)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));
}

#[tokio::test]
async fn delete_user_removes_row_and_reports_result() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();
    assert!(store.delete_user("u1").await.unwrap());
    assert!(store.get_user("u1").await.unwrap().is_none());
    assert!(!store.delete_user("u1").await.unwrap());
}

#[tokio::test]
async fn count_users_tracks_inserts_and_deletes() {
    let (_dir, _backend, store) = make_store().await;
    assert_eq!(store.count_users().await.unwrap(), 0);
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();
    assert_eq!(store.count_users().await.unwrap(), 1);
    store.delete_user("u1").await.unwrap();
    assert_eq!(store.count_users().await.unwrap(), 0);
}

// ---------------------------------------------------------------------
// T5/finding #5: atomic last-admin guard
// ---------------------------------------------------------------------

#[tokio::test]
async fn try_delete_user_unless_last_admin_refuses_the_sole_admin() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "only-admin", Role::Admin))
        .await
        .unwrap();

    assert!(!store.try_delete_user_unless_last_admin("u1").await.unwrap());
    assert!(store.get_user("u1").await.unwrap().is_some());
}

#[tokio::test]
async fn try_delete_user_unless_last_admin_succeeds_with_another_admin() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "admin1", Role::Admin))
        .await
        .unwrap();
    store
        .create_user(&make_user("u2", "admin2", Role::Admin))
        .await
        .unwrap();

    assert!(store.try_delete_user_unless_last_admin("u1").await.unwrap());
    assert!(store.get_user("u1").await.unwrap().is_none());
    assert!(store.get_user("u2").await.unwrap().is_some());
}

#[tokio::test]
async fn try_delete_user_unless_last_admin_allows_deleting_a_member() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "solo-admin", Role::Admin))
        .await
        .unwrap();
    store
        .create_user(&make_user("u2", "some-member", Role::Member))
        .await
        .unwrap();

    assert!(store.try_delete_user_unless_last_admin("u2").await.unwrap());
    assert!(store.get_user("u2").await.unwrap().is_none());
}

#[tokio::test]
async fn try_demote_user_unless_last_admin_refuses_the_sole_admin() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "only-admin", Role::Admin))
        .await
        .unwrap();

    assert!(!store.try_demote_user_unless_last_admin("u1").await.unwrap());
    let found = store.get_user("u1").await.unwrap().unwrap();
    assert_eq!(found.role, Role::Admin);
}

#[tokio::test]
async fn try_demote_user_unless_last_admin_succeeds_with_another_admin() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "admin1", Role::Admin))
        .await
        .unwrap();
    store
        .create_user(&make_user("u2", "admin2", Role::Admin))
        .await
        .unwrap();

    assert!(store.try_demote_user_unless_last_admin("u1").await.unwrap());
    let found = store.get_user("u1").await.unwrap().unwrap();
    assert_eq!(found.role, Role::Member);
    let other = store.get_user("u2").await.unwrap().unwrap();
    assert_eq!(other.role, Role::Admin);
}

#[tokio::test]
async fn try_demote_user_unless_last_admin_is_a_noop_on_a_member() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "solo-admin", Role::Admin))
        .await
        .unwrap();
    store
        .create_user(&make_user("u2", "some-member", Role::Member))
        .await
        .unwrap();

    assert!(!store.try_demote_user_unless_last_admin("u2").await.unwrap());
}
