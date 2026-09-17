mod common;
use axum::http::{Method, StatusCode};
use common::{json_body, make_enforced_app, request_with_bearer, seed_user_with_key};
use localdb_core::auth::Role;

#[tokio::test]
async fn job_management_and_progress_require_admin() {
    let (_dir, state, app) = make_enforced_app().await;
    let member = seed_user_with_key(&state, "reader", Role::Member).await;
    let admin = seed_user_with_key(&state, "admin", Role::Admin).await;
    for (method, path) in [
        (Method::GET, "/v1/jobs"),
        (Method::GET, "/v1/jobs/missing"),
        (Method::DELETE, "/v1/jobs/missing"),
        (Method::GET, "/v1/jobs/missing/events"),
    ] {
        let anon = request_with_bearer(app.clone(), method.clone(), path, None, None).await;
        assert_eq!(anon.status(), StatusCode::UNAUTHORIZED, "{path}");
        let denied =
            request_with_bearer(app.clone(), method.clone(), path, None, Some(&member)).await;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN, "{path}");
        let allowed = request_with_bearer(app.clone(), method, path, None, Some(&admin)).await;
        assert_eq!(
            allowed.status(),
            if path == "/v1/jobs" {
                StatusCode::OK
            } else {
                StatusCode::NOT_FOUND
            },
            "{path}"
        );
    }
}

#[tokio::test]
async fn member_status_and_document_listing_follow_live_grants() {
    let (_dir, state, app) = make_enforced_app().await;
    let member = seed_user_with_key(&state, "reader", Role::Member).await;
    state.add_store("shared", "shared").await.unwrap();
    state.add_store("private", "private").await.unwrap();
    let principal = state.auth().authenticate(&member).await.unwrap();
    let denied = request_with_bearer(
        app.clone(),
        Method::GET,
        "/v1/stores/shared/documents",
        None,
        Some(&member),
    )
    .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let admin = state
        .auth()
        .create_user("admin", Role::Admin)
        .await
        .unwrap();
    state
        .auth()
        .grant_store(
            "shared",
            localdb_core::StoreVisibility::Shared,
            &principal.user_id,
            &admin.id,
        )
        .await
        .unwrap();
    let listed = request_with_bearer(
        app.clone(),
        Method::GET,
        "/v1/stores/shared/documents",
        None,
        Some(&member),
    )
    .await;
    assert_eq!(listed.status(), StatusCode::OK);
    let status =
        request_with_bearer(app.clone(), Method::GET, "/v1/status", None, Some(&member)).await;
    assert_eq!(status.status(), StatusCode::OK);
    let body = json_body(status.into_body()).await;
    assert_eq!(body["store_count"], 1);
    assert!(body.get("database").is_none());
    assert!(body["features"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "refetch"));
    state
        .auth()
        .revoke_store("shared", &principal.user_id)
        .await
        .unwrap();
    let denied = request_with_bearer(
        app,
        Method::GET,
        "/v1/stores/shared/documents",
        None,
        Some(&member),
    )
    .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
}
