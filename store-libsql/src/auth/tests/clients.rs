use super::*;

// ---------------------------------------------------------------------
// OAuth2 dynamic client registration (T7)
// ---------------------------------------------------------------------

fn make_oauth_client(id: &str, redirect_uris: &[&str]) -> localdb_core::auth::OAuthClientRow {
    localdb_core::auth::OAuthClientRow {
        id: id.to_string(),
        client_name: Some("Test Client".to_string()),
        redirect_uris: redirect_uris.iter().map(|s| s.to_string()).collect(),
        created_at: "2026-07-08T00:00:00Z".to_string(),
    }
}

#[tokio::test]
async fn create_and_find_oauth_client_round_trips() {
    let (_dir, _backend, store) = make_store().await;
    let client = make_oauth_client(
        "client-1",
        &["https://app.example.com/cb", "http://127.0.0.1:4000/cb"],
    );
    store.create_oauth_client(&client).await.unwrap();

    let found = store.find_oauth_client("client-1").await.unwrap().unwrap();
    assert_eq!(found, client);
}

#[tokio::test]
async fn find_oauth_client_unknown_id_returns_none() {
    let (_dir, _backend, store) = make_store().await;
    assert!(store
        .find_oauth_client("nonexistent")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn oauth_client_with_no_name_round_trips() {
    let (_dir, _backend, store) = make_store().await;
    let client = localdb_core::auth::OAuthClientRow {
        id: "client-2".to_string(),
        client_name: None,
        redirect_uris: vec!["https://app.example.com/cb".to_string()],
        created_at: "2026-07-08T00:00:00Z".to_string(),
    };
    store.create_oauth_client(&client).await.unwrap();

    let found = store.find_oauth_client("client-2").await.unwrap().unwrap();
    assert_eq!(found.client_name, None);
}
