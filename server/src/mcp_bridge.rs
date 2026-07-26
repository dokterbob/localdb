//! Projects `AppState` into the shapes `mcp::McpHandler` needs to serve the
//! `/mcp` HTTP route: a `StoreProvider` (realtime store resolution) and an
//! `Arc<dyn Embedder>`.
//!
//! The store side is the same lookup `search_service.rs` performs for
//! `/v1/search` per request (and that `cli/src/cmds/surface.rs::run_mcp_async`
//! does client-side for the stdio MCP server) — a thin rearrangement, not new
//! domain logic — except here it is wrapped in a `StoreProvider` impl
//! ([`AppStateStoreProvider`]) so `McpHandler` can call it fresh on every
//! tool invocation instead of once at daemon startup. See
//! `mcp::store_provider` for the full design rationale (D12): a store added
//! later via `POST /v1/stores` is visible on the very next `/mcp` tool call,
//! no daemon restart needed.

use std::sync::Arc;

use async_trait::async_trait;

use localdb_core::config::schema::{EmbeddingPolicy, ProviderConfig};
use localdb_core::embedder::{DocumentChunks, EmbeddedDocument};
use localdb_core::{Embedder, Error};
use mcp::{AvailableStore, StoreDescriptor, StoreProvider};
use tokio::sync::OnceCell;

use crate::state::AppState;

/// A `StoreProvider` over a daemon's `AppState`: each call re-derives the
/// store list from the DB (`effective_config`) and opens a fresh
/// `RetrievalStore` handle per store (`backend().retrieval_store`) — the
/// same per-request lookups `search_service.rs` performs for `/v1/search`.
/// Cloning `AppState` is cheap (it is `Arc`-backed internally), so this type
/// is constructed once at daemon startup and shared across every `/mcp`
/// session via `Arc<dyn StoreProvider>`, but each `available_stores()` call
/// still reflects the database's current contents.
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

/// Build the `Arc<dyn Embedder>` `mcp::McpHandler` needs, from the daemon's
/// current `AppState`.
///
/// Construction is infallible and lazy — see [`LazyEmbedder`] — so this
/// can never be a reason for `start_daemon` to fail or block; any embedder
/// misconfiguration instead surfaces as a normal tool-level error on the
/// first `search` call that actually needs to embed a query.
pub fn build_mcp_embedder(state: &AppState) -> Arc<dyn Embedder> {
    let yaml = state.yaml_config();
    // Server has no `models_dir` override the way the CLI does (see
    // `run_mcp_async`) — mirrors `search_service.rs`'s `create_embedder` call.
    Arc::new(LazyEmbedder::new(
        yaml.defaults.indexing.embedding.clone(),
        yaml.providers.clone(),
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
    inner: OnceCell<Result<Box<dyn Embedder>, Error>>,
}

impl LazyEmbedder {
    fn new(embed_policy: EmbeddingPolicy, providers: Vec<ProviderConfig>) -> Self {
        Self {
            embed_policy,
            providers,
            inner: OnceCell::new(),
        }
    }

    async fn get_or_init(&self) -> &Result<Box<dyn Embedder>, Error> {
        self.inner
            .get_or_init(|| async {
                embed::create_embedder(&self.embed_policy, &self.providers, None)
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
            queue.clone(),
            UrlRefreshScheduler::new(queue),
            crate::auth::AuthMode::Open,
        )
        .await
        .unwrap();
        (dir, state)
    }

    #[tokio::test]
    async fn build_mcp_embedder_succeeds_even_when_embedder_provider_unavailable() {
        // `AppState::new` itself calls `embed::infer_dim_encoding` up front
        // (a static provider/model → (dim, encoding) table lookup, no
        // `ProviderConfig` needed), so an unrecognized provider name would
        // fail state construction, not `build_mcp_embedder`. `perplexity`
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
        let mut yaml_config = RawConfig {
            version: 1,
            server: Default::default(),
            paths: Default::default(),
            defaults: Default::default(),
            providers: vec![],
        };
        yaml_config.defaults.indexing.embedding = EmbeddingPolicy {
            provider: "perplexity".to_string(),
            model: "default".to_string(),
        };
        let (_dir, state) = make_state(yaml_config).await;

        let stores = AppStateStoreProvider::new(state.clone())
            .available_stores()
            .await
            .unwrap();
        let embedder = build_mcp_embedder(&state);

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
        let mut yaml_config = RawConfig {
            version: 1,
            server: Default::default(),
            paths: Default::default(),
            defaults: Default::default(),
            providers: vec![],
        };
        yaml_config.defaults.indexing.embedding = EmbeddingPolicy {
            provider: "fake".to_string(),
            model: "default".to_string(),
        };
        let (_dir, state) = make_state(yaml_config).await;
        state.add_store("notes", "private").await.unwrap();

        let stores = AppStateStoreProvider::new(state.clone())
            .available_stores()
            .await
            .unwrap();
        let embedder = build_mcp_embedder(&state);

        assert_eq!(stores.len(), 1);
        assert_eq!(stores[0].descriptor.name, "notes");
        assert_eq!(embedder.model_id(), "uninitialized");

        embedder.embed_documents(vec![]).await.unwrap();
        assert_ne!(embedder.model_id(), "unavailable");
    }

    #[tokio::test]
    async fn app_state_store_provider_reflects_stores_added_after_construction() {
        // The whole point of T2: the provider is constructed once, but a
        // store added *after* construction (simulating a `POST /v1/stores`
        // arriving after the daemon/router started) must still show up on
        // the next `available_stores()` call, with no restart or
        // reconstruction of the provider itself.
        let (_dir, state) = make_state(RawConfig {
            version: 1,
            server: Default::default(),
            paths: Default::default(),
            defaults: Default::default(),
            providers: vec![],
        })
        .await;
        let provider = AppStateStoreProvider::new(state.clone());

        let before = provider.available_stores().await.unwrap();
        assert!(before.is_empty(), "no stores yet");

        state.add_store("late-store", "private").await.unwrap();

        let after = provider.available_stores().await.unwrap();
        assert_eq!(after.len(), 1, "the newly added store must be visible");
        assert_eq!(after[0].descriptor.name, "late-store");
    }
}
