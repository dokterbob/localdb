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

    // None of these clients ever completes an authorization, so there is
    // always an unused dynamic client to evict — every attempt succeeds
    // (`CapReached` never happens), but the table must still never hold
    // more than `cap` at once: each new insert evicts one of the earlier
    // ones rather than the cap being overshot.
    let mut created = 0;
    let mut cap_reached = 0;
    for handle in handles {
        match handle.await.unwrap() {
            localdb_core::auth::RegisterClientOutcome::Created => created += 1,
            localdb_core::auth::RegisterClientOutcome::CapReached => cap_reached += 1,
        }
    }

    assert_eq!(
        created, attempts,
        "every attempt should succeed, evicting an oldest unused client as needed"
    );
    assert_eq!(cap_reached, 0);

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

// -----------------------------------------------------------------
// Eviction at the cap (specs/05-surfaces.md §3.1): a registration flood of
// fresh, unused clients must not be able to lock everyone else out until
// those rows age past the TTL on their own — the oldest unused client is
// evicted instead of refusing.
// -----------------------------------------------------------------

#[tokio::test]
async fn prune_and_create_oauth_client_evicts_the_oldest_unused_client_at_cap() {
    let (_dir, _backend, store) = make_store().await;
    let mut oldest = make_oauth_client("oldest", &["https://app.example.com/cb"]);
    oldest.created_at = "2026-01-01T00:00:00Z".to_string();
    let mut middle = make_oauth_client("middle", &["https://app.example.com/cb"]);
    middle.created_at = "2026-02-01T00:00:00Z".to_string();
    let mut newest = make_oauth_client("newest", &["https://app.example.com/cb"]);
    newest.created_at = "2026-03-01T00:00:00Z".to_string();
    for client in [&oldest, &middle, &newest] {
        store.create_oauth_client(client).await.unwrap();
    }

    let candidate = make_oauth_client("candidate", &["https://app.example.com/cb"]);
    let outcome = store
        .prune_and_create_oauth_client(
            &candidate,
            localdb_core::auth::LOCALDB_CLI_CLIENT_ID,
            CUTOFF_BEFORE_OLD,
            3,
        )
        .await
        .unwrap();

    assert_eq!(outcome, localdb_core::auth::RegisterClientOutcome::Created);
    assert!(
        store.find_oauth_client("oldest").await.unwrap().is_none(),
        "the oldest unused client must be evicted to free the slot"
    );
    assert!(store.find_oauth_client("middle").await.unwrap().is_some());
    assert!(store.find_oauth_client("newest").await.unwrap().is_some());
    assert!(store
        .find_oauth_client("candidate")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn prune_and_create_oauth_client_evicts_as_many_as_needed_to_reach_the_cap() {
    let (_dir, _backend, store) = make_store().await;
    // Five unused dynamic clients already past a cap of two: the eviction
    // must remove enough of the oldest that the table lands exactly at the
    // cap once the candidate is inserted — not just the single oldest.
    let clients: Vec<_> = [
        ("c1", "2026-01-01T00:00:00Z"),
        ("c2", "2026-01-02T00:00:00Z"),
        ("c3", "2026-01-03T00:00:00Z"),
        ("c4", "2026-01-04T00:00:00Z"),
        ("c5", "2026-01-05T00:00:00Z"),
    ]
    .into_iter()
    .map(|(id, created_at)| {
        let mut client = make_oauth_client(id, &["https://app.example.com/cb"]);
        client.created_at = created_at.to_string();
        client
    })
    .collect();
    for client in &clients {
        store.create_oauth_client(client).await.unwrap();
    }

    let candidate = make_oauth_client("candidate", &["https://app.example.com/cb"]);
    let outcome = store
        .prune_and_create_oauth_client(
            &candidate,
            localdb_core::auth::LOCALDB_CLI_CLIENT_ID,
            CUTOFF_BEFORE_OLD,
            2,
        )
        .await
        .unwrap();

    assert_eq!(outcome, localdb_core::auth::RegisterClientOutcome::Created);
    for id in ["c1", "c2", "c3", "c4"] {
        assert!(
            store.find_oauth_client(id).await.unwrap().is_none(),
            "{id} should have been evicted to reach the cap"
        );
    }
    assert!(
        store.find_oauth_client("c5").await.unwrap().is_some(),
        "the newest survivor must remain"
    );
    assert!(store
        .find_oauth_client("candidate")
        .await
        .unwrap()
        .is_some());

    let mut remaining = 0;
    for id in ["c1", "c2", "c3", "c4", "c5", "candidate"] {
        if store.find_oauth_client(id).await.unwrap().is_some() {
            remaining += 1;
        }
    }
    assert_eq!(remaining, 2, "the table must land exactly at the cap");
}

#[tokio::test]
async fn prune_and_create_oauth_client_refuses_without_evicting_when_used_clients_alone_exceed_the_cap(
) {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("user-1", "alice", Role::Member))
        .await
        .unwrap();
    // Three used clients already over a cap of two: there is no unused
    // client to evict, so the registration must refuse without deleting
    // anything already registered.
    for id in ["used-1", "used-2", "used-3"] {
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
    for id in ["used-1", "used-2", "used-3"] {
        assert!(
            store.find_oauth_client(id).await.unwrap().is_some(),
            "{id} must not be evicted"
        );
    }
    assert!(store
        .find_oauth_client("candidate")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn prune_and_create_oauth_client_never_evicts_a_used_client_even_at_cap() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("user-1", "alice", Role::Member))
        .await
        .unwrap();
    let mut used = make_oauth_client("used", &["https://app.example.com/cb"]);
    used.created_at = "2026-01-01T00:00:00Z".to_string();
    store.create_oauth_client(&used).await.unwrap();
    store
        .create_auth_code(&make_auth_code("used"))
        .await
        .unwrap();
    let mut unused = make_oauth_client("unused", &["https://app.example.com/cb"]);
    unused.created_at = "2026-02-01T00:00:00Z".to_string();
    store.create_oauth_client(&unused).await.unwrap();

    let candidate = make_oauth_client("candidate", &["https://app.example.com/cb"]);
    let outcome = store
        .prune_and_create_oauth_client(
            &candidate,
            localdb_core::auth::LOCALDB_CLI_CLIENT_ID,
            CUTOFF_BEFORE_OLD,
            2,
        )
        .await
        .unwrap();

    assert_eq!(outcome, localdb_core::auth::RegisterClientOutcome::Created);
    assert!(
        store.find_oauth_client("used").await.unwrap().is_some(),
        "a used client must never be evicted, even though it is the oldest"
    );
    assert!(
        store.find_oauth_client("unused").await.unwrap().is_none(),
        "the only evictable (unused) client must be the one freed"
    );
    assert!(store
        .find_oauth_client("candidate")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn prune_and_create_oauth_client_never_evicts_the_builtin_client() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("user-1", "alice", Role::Member))
        .await
        .unwrap();
    // The built-in client's epoch `created_at` (seeded by `create_schema`)
    // would make it the oldest candidate for eviction by age alone if it
    // were not excluded by id.
    let mut used = make_oauth_client("used", &["https://app.example.com/cb"]);
    used.created_at = "2026-01-01T00:00:00Z".to_string();
    store.create_oauth_client(&used).await.unwrap();
    store
        .create_auth_code(&make_auth_code("used"))
        .await
        .unwrap();

    // cap=1 counts only dynamic clients, so "used" alone already fills it;
    // with no unused dynamic client to evict, this must refuse.
    let candidate = make_oauth_client("candidate", &["https://app.example.com/cb"]);
    let outcome = store
        .prune_and_create_oauth_client(
            &candidate,
            localdb_core::auth::LOCALDB_CLI_CLIENT_ID,
            CUTOFF_BEFORE_OLD,
            1,
        )
        .await
        .unwrap();

    assert_eq!(
        outcome,
        localdb_core::auth::RegisterClientOutcome::CapReached
    );
    assert!(store
        .find_oauth_client(localdb_core::auth::LOCALDB_CLI_CLIENT_ID)
        .await
        .unwrap()
        .is_some());
    assert!(store.find_oauth_client("used").await.unwrap().is_some());
}

// -----------------------------------------------------------------
// Deleting a user cascades to their `auth_codes` (`ON DELETE CASCADE`) — a
// client whose only completed authorization belonged to a deleted user
// becomes unused again, and therefore prunable/evictable. See
// `core/src/auth/store/tests.rs`'s identical test for `FakeAuthStore`.
// -----------------------------------------------------------------

#[tokio::test]
async fn deleting_a_user_cascades_their_auth_codes_so_the_client_they_used_becomes_evictable() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("user-1", "alice", Role::Member))
        .await
        .unwrap();
    let mut used = make_oauth_client("used", &["https://app.example.com/cb"]);
    used.created_at = OLD_CREATED_AT.to_string();
    store.create_oauth_client(&used).await.unwrap();
    store
        .create_auth_code(&make_auth_code("used"))
        .await
        .unwrap();

    // While "used" is still used, it alone fills a cap of 1 and there is no
    // unused dynamic client to evict.
    let candidate_1 = make_oauth_client("candidate-1", &["https://app.example.com/cb"]);
    let outcome_1 = store
        .prune_and_create_oauth_client(
            &candidate_1,
            localdb_core::auth::LOCALDB_CLI_CLIENT_ID,
            CUTOFF_AFTER_OLD,
            1,
        )
        .await
        .unwrap();
    assert_eq!(
        outcome_1,
        localdb_core::auth::RegisterClientOutcome::CapReached
    );

    assert!(store.delete_user("user-1").await.unwrap());

    // The auth code (and with it, "used"'s used-ness) is gone, so it is now
    // past the TTL and prunable.
    let candidate_2 = make_oauth_client("candidate-2", &["https://app.example.com/cb"]);
    let outcome_2 = store
        .prune_and_create_oauth_client(
            &candidate_2,
            localdb_core::auth::LOCALDB_CLI_CLIENT_ID,
            CUTOFF_AFTER_OLD,
            1,
        )
        .await
        .unwrap();
    assert_eq!(
        outcome_2,
        localdb_core::auth::RegisterClientOutcome::Created
    );
    assert!(
        store.find_oauth_client("used").await.unwrap().is_none(),
        "the client's only auth code was deleted along with its user"
    );
    assert!(store
        .find_oauth_client("candidate-2")
        .await
        .unwrap()
        .is_some());
}
