//! Resolves the daemon’s current stores for each MCP call and constructs its lazy embedder.

use std::sync::Arc;

use async_trait::async_trait;
use localdb_core::config::schema::{EmbeddingPolicy, ProviderConfig};
use localdb_core::embedder::{DocumentChunks, EmbeddedDocument};
use localdb_core::{Embedder, Error};
use mcp::{AvailableStore, StoreDescriptor, StoreProvider};
pub struct AppStateStoreProvider {
    state: AppState,
}

impl AppStateStoreProvider {
    /// Wrap the daemon's `AppState`.
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

#[async_trait]
impl StoreProvider for AppStateStoreProvider {
    async fn available_stores(&self) -> Result<Vec<AvailableStore>, Error> {
        let effective = self.state.effective_config().await?;

        let mut stores = Vec::with_capacity(effective.stores.len());
        for store_cfg in &effective.stores {
            let descriptor = StoreDescriptor {
                id: store_cfg.id.clone(),
                name: store_cfg.name.clone(),
                visibility: store_cfg.visibility.clone(),
            };
            let handle = self.state.backend().retrieval_store(&store_cfg.id).await?;
            stores.push(AvailableStore::from_arc(descriptor, handle));
        }

        Ok(stores)
    }
}

use tokio::sync::OnceCell;

use crate::state::AppState;

/// Project the current stores and a lazy embedder for callers needing both.
pub async fn build_available_stores(
    state: &AppState,
) -> Result<(Vec<AvailableStore>, Arc<dyn Embedder>), Error> {
    let stores = AppStateStoreProvider::new(state.clone())
        .available_stores()
        .await?;
    Ok((stores, build_mcp_embedder(state)))
}

pub fn build_mcp_embedder(state: &AppState) -> Arc<dyn Embedder> {
    let yaml = state.yaml_config();
    Arc::new(LazyEmbedder::new(
        yaml.defaults.indexing.embedding.clone(),
        yaml.providers.clone(),
        state.models_dir().to_path_buf(),
        fetch::http::HttpSettings::from(&yaml.http),
    ))
}

/// Defers `embed::create_embedder` (which, for the default `local`/
/// `local-onnx`/`local-coreml` providers, can synchronously download or load
/// a several-hundred-MB model) to the first `/mcp` `search` call instead of
/// running it inline during `start_daemon` — otherwise even unrelated
/// `/v1/*` routes would be unreachable until that finishes. The result
/// (success or failure) is cached in `inner` so construction runs at most
/// once regardless of how many searches follow.
struct LazyEmbedder {
    embed_policy: EmbeddingPolicy,
    providers: Vec<ProviderConfig>,
    models_dir: std::path::PathBuf,
    /// Operator's `http:` config, converted once at construction time (issue
    /// #207 adversarial review, finding 1) — without this, a hosted
    /// provider's client would silently fall back to
    /// `fetch::http::HttpSettings::default()` no matter what the operator
    /// set under `http:` in `config.yaml`. Snapshotted here rather than
    /// re-read from `AppState` on each `get_or_init` call because this
    /// struct already snapshots `embed_policy`/`providers` the same way —
    /// see the module doc for why this whole projection is a startup-time
    /// snapshot, not a per-session rebuild.
    http_settings: fetch::http::HttpSettings,
    inner: OnceCell<Result<Box<dyn Embedder>, Error>>,
}

impl LazyEmbedder {
    fn new(
        embed_policy: EmbeddingPolicy,
        providers: Vec<ProviderConfig>,
        models_dir: std::path::PathBuf,
        http_settings: fetch::http::HttpSettings,
    ) -> Self {
        Self {
            embed_policy,
            providers,
            models_dir,
            http_settings,
            inner: OnceCell::new(),
        }
    }

    async fn get_or_init(&self) -> &Result<Box<dyn Embedder>, Error> {
        self.inner
            .get_or_init(|| async {
                embed::create_embedder(
                    &self.embed_policy,
                    &self.providers,
                    Some(&self.models_dir),
                    &self.http_settings,
                )
                .map_err(Error::from)
            })
            .await
    }
}

#[async_trait::async_trait]
impl Embedder for LazyEmbedder {
    async fn embed_documents(
        &self,
        docs: Vec<DocumentChunks>,
    ) -> Result<Vec<EmbeddedDocument>, Error> {
        match self.get_or_init().await {
            Ok(e) => e.embed_documents(docs).await,
            // `SearchOrchestrator::query`'s existing error handling turns
            // this into a normal tool-level `CallToolResult` error (see
            // `tools::tool_search`), not a panic or daemon-wide failure.
            Err(e) => Err(e.clone()),
        }
    }

    /// Only ever called in tests (no production caller needs a dimension
    /// before the first `embed_documents` call) — placeholder until the
    /// real embedder is constructed.
    fn embedding_dim(&self) -> usize {
        self.inner
            .get()
            .and_then(|r| r.as_ref().ok())
            .map(|e| e.embedding_dim())
            .unwrap_or(0)
    }

    /// Same caveat as `embedding_dim`.
    fn model_id(&self) -> &str {
        self.inner
            .get()
            .and_then(|r| r.as_ref().ok())
            .map(|e| e.model_id())
            .unwrap_or("uninitialized")
    }
}

#[cfg(test)]
mod tests {
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
}
