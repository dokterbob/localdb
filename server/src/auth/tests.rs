use super::*;
use localdb_core::auth::{hash_secret, Role};
use localdb_core::config::schema::RawConfig;

async fn make_state(auth_mode: AuthMode) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let mut yaml_config = RawConfig {
        version: 1,
        server: Default::default(),
        paths: Default::default(),
        defaults: Default::default(),
        providers: vec![],
        ..Default::default()
    };
    yaml_config.defaults.indexing.embedding = localdb_core::config::schema::EmbeddingPolicy {
        provider: "fake".to_string(),
        model: "default".to_string(),
    };
    let queue = crate::job_queue::JobQueue::new();
    let state = AppState::new(
        yaml_config,
        dir.path().to_path_buf(),
        dir.path().to_path_buf().join("models"),
        queue.clone(),
        crate::scheduler::UrlRefreshScheduler::new(queue),
        auth_mode,
    )
    .await
    .unwrap();
    (dir, state)
}

#[tokio::test]
async fn setup_code_generated_when_enforced_and_no_users() {
    let (_dir, state) = make_state(AuthMode::Enforced).await;

    let code = generate_setup_code_if_needed(&state).await.unwrap();

    let code = code.expect("enforced + zero users must yield a setup code");
    assert!(
        code.starts_with(localdb_core::auth::TOKEN_PREFIX),
        "setup code should be a minted ldb_ secret, got: {code}"
    );
    // The hash — and only the hash — is held in AppState for T4.
    assert_eq!(
        state.setup_code_hash().as_deref(),
        Some(hash_secret(&code).as_str()),
        "AppState must hold the blake3 hash of the printed plaintext"
    );
}

#[tokio::test]
async fn setup_code_not_generated_when_an_admin_exists() {
    let (_dir, state) = make_state(AuthMode::Enforced).await;
    state
        .auth()
        .create_user("admin", Role::Admin)
        .await
        .unwrap();

    let code = generate_setup_code_if_needed(&state).await.unwrap();

    assert!(
        code.is_none(),
        "an existing admin must suppress the setup code"
    );
    assert!(state.setup_code_hash().is_none());
}

/// Finding #5 regression: a first user created *without* `--admin` (a
/// plain `Role::Member`) must NOT suppress the setup code — the old
/// "any user exists" check would otherwise leave the daemon
/// auth-enforced with zero admins and no API-reachable way to create
/// one.
#[tokio::test]
async fn setup_code_still_generated_when_only_member_users_exist() {
    let (_dir, state) = make_state(AuthMode::Enforced).await;
    state.auth().create_user("bob", Role::Member).await.unwrap();

    let code = generate_setup_code_if_needed(&state).await.unwrap();

    let code = code.expect("a member-only instance must still get a setup code");
    assert!(code.starts_with(localdb_core::auth::TOKEN_PREFIX));
    assert_eq!(
        state.setup_code_hash().as_deref(),
        Some(hash_secret(&code).as_str())
    );
}

#[tokio::test]
async fn setup_code_not_generated_in_open_mode() {
    let (_dir, state) = make_state(AuthMode::Open).await;

    let code = generate_setup_code_if_needed(&state).await.unwrap();

    assert!(code.is_none(), "open mode must not mint a setup code");
    assert!(state.setup_code_hash().is_none());
}

#[tokio::test]
async fn pending_bootstrap_survives_restart_and_rotates_setup_code() {
    let (_dir, state) = make_state(AuthMode::Enforced).await;
    let first_code = generate_setup_code_if_needed(&state)
        .await
        .unwrap()
        .unwrap();
    let admin = state.auth().begin_bootstrap().await.unwrap();
    // A new AppState uses the same persisted data but no in-memory setup hash.
    let queue = crate::job_queue::JobQueue::new();
    let mut config = RawConfig::default();
    config.defaults.indexing.embedding.provider = "fake".into();
    config.defaults.indexing.embedding.model = "default".into();
    let restarted = AppState::new(
        config,
        _dir.path().to_path_buf(),
        _dir.path().join("models"),
        queue.clone(),
        crate::scheduler::UrlRefreshScheduler::new(queue),
        AuthMode::Enforced,
    )
    .await
    .unwrap();
    let new_code = generate_setup_code_if_needed(&restarted)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(first_code, new_code);
    assert_eq!(restarted.setup_code_hash(), Some(hash_secret(&new_code)));
    assert_eq!(
        restarted.auth().begin_bootstrap().await.unwrap().id,
        admin.id
    );
    let token = restarted.auth().issue_api_key(&admin.id).await.unwrap();
    let principal = restarted.auth().authenticate(&token.secret).await.unwrap();
    restarted
        .auth()
        .complete_bootstrap(&principal)
        .await
        .unwrap();
    assert!(generate_setup_code_if_needed(&restarted)
        .await
        .unwrap()
        .is_none());
}
