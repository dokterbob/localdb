//! Projects `AppState` into the `Vec<mcp::AvailableStore>` + `Arc<dyn
//! Embedder>` shape `mcp::McpHandler` needs to serve the `/mcp` HTTP route.
//!
//! This is the same store/embedder projection `search_service.rs` performs
//! for `/v1/search` (and that `cli/src/cmds/surface.rs::run_mcp_async` does
//! client-side for the stdio MCP server) — a thin rearrangement, not new
//! domain logic.
//!
//! Called exactly once, from `daemon::start_daemon`, and the result is
//! handed to `build_router` to construct the `/mcp` service. This is a
//! deliberate startup-time snapshot rather than a per-session rebuild:
//! rmcp's HTTP service-factory closure is synchronous
//! (`Fn() -> Result<S, io::Error>`), so there is no hook to redo these async
//! `AppState` lookups per session. A store added later via `/v1/stores` is
//! therefore invisible over MCP until the daemon restarts — an accepted,
//! documented gap (specs/05-surfaces.md §4), not a bug to work around here.

use std::sync::Arc;

use localdb_core::config::schema::{EmbeddingPolicy, ProviderConfig};
use localdb_core::embedder::{DocumentChunks, EmbeddedDocument};
use localdb_core::{Embedder, Error};
use mcp::{AvailableStore, StoreDescriptor};
use tokio::sync::OnceCell;

use crate::state::AppState;

/// Build the `(stores, embedder)` pair `mcp::build_streamable_http_service`
/// needs, from the daemon's current `AppState`.
///
/// Only genuine backend failures (`effective_config`/`retrieval_store`)
/// return `Err` here and abort daemon startup, matching
/// `build_daemon_state`'s existing fail-fast behavior for a broken backend.
/// Embedder construction is deliberately deferred — see [`LazyEmbedder`] —
/// so it can never be a reason for this function (and thus `start_daemon`)
/// to fail or block.
pub async fn build_available_stores(
    state: &AppState,
) -> Result<(Vec<AvailableStore>, Arc<dyn Embedder>), Error> {
    let effective = state.effective_config().await?;

    let mut stores = Vec::with_capacity(effective.stores.len());
    for store_cfg in &effective.stores {
        let descriptor = StoreDescriptor {
            id: store_cfg.id.clone(),
            name: store_cfg.name.clone(),
            visibility: store_cfg.visibility.clone(),
        };
        let handle = state.backend().retrieval_store(&store_cfg.id).await?;
        stores.push(AvailableStore::from_arc(descriptor, handle));
    }

    let yaml = state.yaml_config().await;
    let embedder: Arc<dyn Embedder> = Arc::new(LazyEmbedder::new(
        yaml.defaults.indexing.embedding.clone(),
        yaml.providers.clone(),
        state.models_dir().to_path_buf(),
        fetch::http::HttpSettings::from(&yaml.http),
    ));

    Ok((stores, embedder))
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
mod tests;
