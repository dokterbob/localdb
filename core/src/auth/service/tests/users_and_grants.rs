use super::*;

#[tokio::test]
async fn create_user_then_issue_api_key_authenticates() {
    let svc = service();
    let user = svc.create_user("alice", Role::Admin).await.unwrap();
    let issued = svc.issue_api_key(&user.id).await.unwrap();
    let principal = svc.authenticate(&issued.secret).await.unwrap();
    assert_eq!(principal.user_id, user.id);
    assert_eq!(principal.role, Role::Admin);
    assert_eq!(principal.access, StoreAccess::All);
}

#[tokio::test]
async fn duplicate_user_name_rejected() {
    let svc = service();
    svc.create_user("alice", Role::Admin).await.unwrap();
    let err = svc.create_user("alice", Role::Member).await.unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));
}

#[tokio::test]
async fn grant_policy_matrix() {
    let svc = service();
    let admin = Principal {
        user_id: "admin-1".into(),
        name: "admin".into(),
        role: Role::Admin,
        access: StoreAccess::All,
    };
    let member_with_grant = Principal {
        user_id: "m1".into(),
        name: "member1".into(),
        role: Role::Member,
        access: StoreAccess::Granted(["docs".to_string()].into_iter().collect()),
    };
    let member_no_grant = Principal {
        user_id: "m2".into(),
        name: "member2".into(),
        role: Role::Member,
        access: StoreAccess::Granted(Default::default()),
    };

    // Admin sees everything, private or shared.
    assert!(svc.can_read_store(&admin, "docs", StoreVisibility::Shared));
    assert!(svc.can_read_store(&admin, "secret", StoreVisibility::Private));

    // Member with a grant on a shared store: yes.
    assert!(svc.can_read_store(&member_with_grant, "docs", StoreVisibility::Shared));
    // Member without a grant: no.
    assert!(!svc.can_read_store(&member_no_grant, "docs", StoreVisibility::Shared));
    // Member with a grant attempting the same store as private: no —
    // private is admin-only regardless of any grant.
    assert!(!svc.can_read_store(&member_with_grant, "docs", StoreVisibility::Private));
}

#[tokio::test]
async fn grant_store_rejects_private_visibility() {
    let svc = service();
    let err = svc
        .grant_store(
            "secret-store",
            StoreVisibility::Private,
            "user-1",
            "admin-1",
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Forbidden { .. }));
}

#[tokio::test]
async fn grant_store_allows_shared_visibility() {
    let svc = service();
    svc.grant_store("docs", StoreVisibility::Shared, "user-1", "admin-1")
        .await
        .unwrap();
    let grants = svc.store.list_grants_for_user("user-1").await.unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].store_name, "docs");
}

#[tokio::test]
async fn revoke_store_removes_grant() {
    let svc = service();
    svc.grant_store("docs", StoreVisibility::Shared, "user-1", "admin-1")
        .await
        .unwrap();
    let removed = svc.revoke_store("docs", "user-1").await.unwrap();
    assert!(removed);
    let grants = svc.store.list_grants_for_user("user-1").await.unwrap();
    assert!(grants.is_empty());
}

#[tokio::test]
async fn member_principal_reflects_grants_after_authenticate() {
    let svc = service();
    let user = svc.create_user("hank", Role::Member).await.unwrap();
    svc.grant_store("docs", StoreVisibility::Shared, &user.id, "admin-1")
        .await
        .unwrap();
    let issued = svc.issue_api_key(&user.id).await.unwrap();
    let principal = svc.authenticate(&issued.secret).await.unwrap();
    assert!(principal.can_read_store("docs", StoreVisibility::Shared));
    assert!(!principal.can_read_store("other", StoreVisibility::Shared));
}

#[tokio::test]
async fn delete_user_refuses_the_last_admin() {
    let svc = service();
    let admin = svc.create_user("only-admin", Role::Admin).await.unwrap();

    let err = svc.delete_user(&admin.id).await.unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));
    assert!(svc.store.get_user(&admin.id).await.unwrap().is_some());
}

#[tokio::test]
async fn delete_user_allows_a_non_last_admin() {
    let svc = service();
    let admin1 = svc.create_user("admin1", Role::Admin).await.unwrap();
    let admin2 = svc.create_user("admin2", Role::Admin).await.unwrap();

    assert!(svc.delete_user(&admin1.id).await.unwrap());
    assert!(svc.store.get_user(&admin2.id).await.unwrap().is_some());
}

#[tokio::test]
async fn delete_user_allows_deleting_a_member() {
    let svc = service();
    let admin = svc.create_user("solo-admin", Role::Admin).await.unwrap();
    let member = svc.create_user("some-member", Role::Member).await.unwrap();

    assert!(svc.delete_user(&member.id).await.unwrap());
    // The sole admin is untouched and still present.
    assert!(svc.store.get_user(&admin.id).await.unwrap().is_some());
}

#[tokio::test]
async fn set_user_role_refuses_to_demote_the_last_admin() {
    let svc = service();
    let admin = svc.create_user("only-admin2", Role::Admin).await.unwrap();

    let err = svc
        .set_user_role(&admin.id, Role::Member)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));
    let reloaded = svc.store.get_user(&admin.id).await.unwrap().unwrap();
    assert_eq!(reloaded.role, Role::Admin, "role must be unchanged");
}

#[tokio::test]
async fn set_user_role_allows_demoting_a_non_last_admin() {
    let svc = service();
    let admin1 = svc.create_user("admin1b", Role::Admin).await.unwrap();
    let admin2 = svc.create_user("admin2b", Role::Admin).await.unwrap();

    svc.set_user_role(&admin1.id, Role::Member).await.unwrap();
    let reloaded = svc.store.get_user(&admin1.id).await.unwrap().unwrap();
    assert_eq!(reloaded.role, Role::Member);
    // The remaining admin is unaffected.
    let admin2_reloaded = svc.store.get_user(&admin2.id).await.unwrap().unwrap();
    assert_eq!(admin2_reloaded.role, Role::Admin);
}

#[tokio::test]
async fn set_user_role_allows_promoting_a_member_to_admin() {
    let svc = service();
    svc.create_user("solo-admin2", Role::Admin).await.unwrap();
    let member = svc.create_user("promotable", Role::Member).await.unwrap();

    svc.set_user_role(&member.id, Role::Admin).await.unwrap();
    let reloaded = svc.store.get_user(&member.id).await.unwrap().unwrap();
    assert_eq!(reloaded.role, Role::Admin);
}
