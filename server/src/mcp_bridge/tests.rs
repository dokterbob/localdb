use super::*;
use crate::job_queue::JobQueue;
use crate::scheduler::UrlRefreshScheduler;
use localdb_core::config::schema::RawConfig;

async fn make_state(yaml_config: RawConfig) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let queue = JobQueue::new();
    let state = AppState::new(
        yaml_config,
        dir.path().to_path_buf(),
        dir.path().join("models"),
        queue.clone(),
        UrlRefreshScheduler::new(queue),
        crate::auth::AuthMode::Open,
    )
    .await
    .unwrap();
    (dir, state)
}

#[tokio::test]
async fn build_available_stores_succeeds_even_when_embedder_provider_unavailable() {
    // `AppState::new` itself calls `embed::infer_dim_encoding` up front
    // (a static provider/model → (dim, encoding) table lookup, no
    // `ProviderConfig` needed), so an unrecognized provider name would
    // fail state construction, not `build_available_stores`. `perplexity`
    // with no matching `providers:` entry instead passes that lookup
    // (it only checks provider/model name) but deterministically fails
    // `create_embedder` at the `ProviderNotConfigured` step, in any
    // build — unlike `local`, whose availability depends on which
    // workspace members are compiled alongside `server` (`cargo build
    // --workspace` unifies `embed`'s `local-onnx`/`local-coreml`
    // features in from `cli`'s unconditional/macOS-gated dependency
    // edges, so `local` can silently succeed here too).
    //
    // Construction is lazy now, so this succeeds unconditionally —
    // the failure only surfaces on the first `embed_documents` call,
    // asserted below with the mapped error (not a hard-coded
    // `ModelMissing`, the Codex-flagged bug this test now pins).
    let mut yaml_config = RawConfig::default();
    yaml_config.defaults.indexing.embedding = EmbeddingPolicy {
        provider: "perplexity".to_string(),
        model: "default".to_string(),
    };
    let (_dir, state) = make_state(yaml_config).await;

    let (stores, embedder) = build_available_stores(&state).await.unwrap();

    assert!(stores.is_empty());
    assert_eq!(embedder.model_id(), "uninitialized");
    assert_eq!(embedder.embedding_dim(), 0);
    let err = embedder.embed_documents(vec![]).await.unwrap_err();
    assert!(
        matches!(err, Error::InvalidConfig { .. }),
        "expected InvalidConfig (mapped from EmbedError::ProviderNotConfigured), got: {err:?}"
    );
}

#[tokio::test]
async fn returns_real_embedder_and_store_handles_when_provider_available() {
    let mut yaml_config = RawConfig::default();
    yaml_config.defaults.indexing.embedding = EmbeddingPolicy {
        provider: "fake".to_string(),
        model: "default".to_string(),
    };
    let (_dir, state) = make_state(yaml_config).await;
    state.add_store("notes", "private").await.unwrap();

    let (stores, embedder) = build_available_stores(&state).await.unwrap();

    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].descriptor.name, "notes");
    assert_eq!(embedder.model_id(), "uninitialized");

    embedder.embed_documents(vec![]).await.unwrap();
    assert_ne!(embedder.model_id(), "unavailable");
}
