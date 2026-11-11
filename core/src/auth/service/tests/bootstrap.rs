use super::*;

#[tokio::test]
async fn begin_bootstrap_succeeds_when_a_member_already_holds_the_name_admin() {
    let svc = service();
    svc.create_user("admin", Role::Member).await.unwrap();

    let bootstrapped = svc.begin_bootstrap().await.unwrap();
    assert_eq!(bootstrapped.name, "admin-2");
    assert_eq!(bootstrapped.role, Role::Admin);
    assert!(svc.bootstrap_needed().await.unwrap());

    let pending = svc.store.pending_bootstrap_user().await.unwrap().unwrap();
    assert_eq!(pending.id, bootstrapped.id);
}

#[tokio::test]
async fn begin_bootstrap_uses_the_plain_name_when_unused() {
    let svc = service();
    let bootstrapped = svc.begin_bootstrap().await.unwrap();
    assert_eq!(bootstrapped.name, "admin");
}
