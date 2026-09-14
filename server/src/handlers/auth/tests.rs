use super::*;

#[tokio::test]
async fn get_me_serializes_local_trust_as_admin_all() {
    let response = get_me(Some(Extension(Principal::local_trust())))
        .await
        .unwrap();
    let value = serde_json::to_value(&response.0).unwrap();
    assert_eq!(value["user_id"], "local");
    assert_eq!(value["name"], "local");
    assert_eq!(value["role"], "admin");
    assert_eq!(value["store_access"], "all");
}

#[tokio::test]
async fn get_me_serializes_member_grants_sorted() {
    let principal = Principal {
        user_id: "u1".into(),
        name: "bob".into(),
        role: Role::Member,
        access: StoreAccess::Granted(
            ["zeta".to_string(), "alpha".to_string()]
                .into_iter()
                .collect(),
        ),
    };
    let response = get_me(Some(Extension(principal))).await.unwrap();
    let value = serde_json::to_value(&response.0).unwrap();
    assert_eq!(value["role"], "member");
    assert_eq!(
        value["store_access"]["granted"]["stores"],
        serde_json::json!(["alpha", "zeta"])
    );
}

#[tokio::test]
async fn get_me_fails_closed_without_principal() {
    let err = get_me(None).await.unwrap_err();
    assert!(matches!(err.0, CoreError::Unauthorized { .. }));
}
