use super::*;

// ---------------------------------------------------------------------
// OAuth2 dynamic client registration
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

// ---------------------------------------------------------------------
// `prune_and_create_oauth_client` (specs/05-surfaces.md §3.1): the
// registration cap/TTL atomic prune-then-insert.
// ---------------------------------------------------------------------

/// A built-in `LOCALDB_CLI_CLIENT_ID` row is seeded by `create_schema`
/// (`created_at: "1970-01-01T00:00:00Z"`), so every test below registers
/// against a store that already has exactly one (unpruneable, non-counted)
/// `oauth_clients` row before it does anything.
const OLD_CREATED_AT: &str = "2000-01-01T00:00:00Z";
const CUTOFF_AFTER_OLD: &str = "2010-01-01T00:00:00Z";
const CUTOFF_BEFORE_OLD: &str = "1990-01-01T00:00:00Z";

fn make_auth_code(client_id: &str) -> localdb_core::auth::AuthCodeRow {
    localdb_core::auth::AuthCodeRow {
        id: format!("code-for-{client_id}"),
        client_id: client_id.to_string(),
        user_id: "user-1".to_string(),
        code_hash: format!("hash-{client_id}"),
        code_challenge: "challenge".to_string(),
        code_challenge_method: "S256".to_string(),
        redirect_uri: "https://app.example.com/cb".to_string(),
        expires_at: "2099-01-01T00:00:00Z".to_string(),
        consumed_at: None,
        created_at: OLD_CREATED_AT.to_string(),
        minted_by_family: None,
    }
}

#[tokio::test]
async fn prune_and_create_oauth_client_inserts_when_under_cap() {
    let (_dir, _backend, store) = make_store().await;
    let client = make_oauth_client("new-client", &["https://app.example.com/cb"]);

    let outcome = store
        .prune_and_create_oauth_client(
            &client,
            localdb_core::auth::LOCALDB_CLI_CLIENT_ID,
            CUTOFF_BEFORE_OLD,
            5,
        )
        .await
        .unwrap();

    assert_eq!(outcome, localdb_core::auth::RegisterClientOutcome::Created);
    assert!(store
        .find_oauth_client("new-client")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn prune_and_create_oauth_client_refuses_when_cap_reached_by_used_clients() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("user-1", "alice", Role::Member))
        .await
        .unwrap();
    // Two "used" clients (each with an issued auth code) at the cap — used
    // clients are never pruned regardless of age, so a third registration
    // must be refused.
    for id in ["used-1", "used-2"] {
        let mut client = make_oauth_client(id, &["https://app.example.com/cb"]);
        client.created_at = OLD_CREATED_AT.to_string();
        store.create_oauth_client(&client).await.unwrap();
        store.create_auth_code(&make_auth_code(id)).await.unwrap();
    }

    let candidate = make_oauth_client("candidate", &["https://app.example.com/cb"]);
    let outcome = store
        .prune_and_create_oauth_client(
            &candidate,
            localdb_core::auth::LOCALDB_CLI_CLIENT_ID,
            CUTOFF_AFTER_OLD,
            2,
        )
        .await
        .unwrap();

    assert_eq!(
        outcome,
        localdb_core::auth::RegisterClientOutcome::CapReached
    );
    assert!(store.find_oauth_client("used-1").await.unwrap().is_some());
    assert!(store.find_oauth_client("used-2").await.unwrap().is_some());
    assert!(store
        .find_oauth_client("candidate")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn prune_and_create_oauth_client_prunes_unused_old_clients_then_succeeds() {
    let (_dir, _backend, store) = make_store().await;
    for id in ["stale-1", "stale-2"] {
        let mut client = make_oauth_client(id, &["https://app.example.com/cb"]);
        client.created_at = OLD_CREATED_AT.to_string();
        store.create_oauth_client(&client).await.unwrap();
    }

    let candidate = make_oauth_client("fresh", &["https://app.example.com/cb"]);
    let outcome = store
        .prune_and_create_oauth_client(
            &candidate,
            localdb_core::auth::LOCALDB_CLI_CLIENT_ID,
            CUTOFF_AFTER_OLD,
            2,
        )
        .await
        .unwrap();

    assert_eq!(outcome, localdb_core::auth::RegisterClientOutcome::Created);
    assert!(store.find_oauth_client("stale-1").await.unwrap().is_none());
    assert!(store.find_oauth_client("stale-2").await.unwrap().is_none());
    assert!(store.find_oauth_client("fresh").await.unwrap().is_some());
}

#[tokio::test]
async fn prune_and_create_oauth_client_never_prunes_the_builtin_client() {
    let (_dir, _backend, store) = make_store().await;
    let client = make_oauth_client("new-client", &["https://app.example.com/cb"]);

    // A cutoff far in the future would make the built-in client's epoch
    // `created_at` eligible by age alone — it must still survive because
    // `prune_and_create_oauth_client` excludes `builtin_client_id` by id.
    store
        .prune_and_create_oauth_client(
            &client,
            localdb_core::auth::LOCALDB_CLI_CLIENT_ID,
            "2099-01-01T00:00:00Z",
            5,
        )
        .await
        .unwrap();

    assert!(store
        .find_oauth_client(localdb_core::auth::LOCALDB_CLI_CLIENT_ID)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn prune_and_create_oauth_client_commits_the_prune_even_when_still_at_cap() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("user-1", "alice", Role::Member))
        .await
        .unwrap();
    let mut stale = make_oauth_client("stale", &["https://app.example.com/cb"]);
    stale.created_at = OLD_CREATED_AT.to_string();
    store.create_oauth_client(&stale).await.unwrap();
    let used = make_oauth_client("used", &["https://app.example.com/cb"]);
    store.create_oauth_client(&used).await.unwrap();
    store
        .create_auth_code(&make_auth_code("used"))
        .await
        .unwrap();

    // cap=1: pruning removes "stale", leaving just "used" — still at the
    // cap, so the candidate is refused, but "stale" must stay pruned.
    let candidate = make_oauth_client("candidate", &["https://app.example.com/cb"]);
    let outcome = store
        .prune_and_create_oauth_client(
            &candidate,
            localdb_core::auth::LOCALDB_CLI_CLIENT_ID,
            CUTOFF_AFTER_OLD,
            1,
        )
        .await
        .unwrap();

    assert_eq!(
        outcome,
        localdb_core::auth::RegisterClientOutcome::CapReached
    );
    assert!(store.find_oauth_client("stale").await.unwrap().is_none());
    assert!(store.find_oauth_client("used").await.unwrap().is_some());
}

#[tokio::test]
async fn prune_and_create_oauth_client_concurrent_registrations_never_overshoot_the_cap() {
    let (_dir, _backend, store) = make_store().await;
    let store = std::sync::Arc::new(store);
    let cap: u64 = 3;
    let attempts = 20;

    let mut handles = Vec::new();
    for i in 0..attempts {
        let store = std::sync::Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            let client =
                make_oauth_client(&format!("concurrent-{i}"), &["https://app.example.com/cb"]);
            store
                .prune_and_create_oauth_client(
                    &client,
                    localdb_core::auth::LOCALDB_CLI_CLIENT_ID,
                    CUTOFF_BEFORE_OLD,
                    cap,
                )
                .await
                .unwrap()
        }));
    }

    let mut created = 0;
    let mut cap_reached = 0;
    for handle in handles {
        match handle.await.unwrap() {
            localdb_core::auth::RegisterClientOutcome::Created => created += 1,
            localdb_core::auth::RegisterClientOutcome::CapReached => cap_reached += 1,
        }
    }

    assert_eq!(
        created, cap as usize,
        "exactly `cap` registrations should have succeeded"
    );
    assert_eq!(cap_reached, attempts - cap as usize);

    let mut actually_present = 0;
    for i in 0..attempts {
        if store
            .find_oauth_client(&format!("concurrent-{i}"))
            .await
            .unwrap()
            .is_some()
        {
            actually_present += 1;
        }
    }
    assert_eq!(
        actually_present, cap as usize,
        "the table must never hold more than `cap` dynamically registered clients"
    );
}
