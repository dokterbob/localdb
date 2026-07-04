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

use localdb_core::embedder::{DocumentChunks, EmbeddedDocument};
use localdb_core::{Embedder, Error};
use mcp::{AvailableStore, StoreDescriptor};

use crate::state::AppState;

/// Build the `(stores, embedder)` pair `mcp::build_streamable_http_service`
/// needs, from the daemon's current `AppState`.
///
/// Only genuine backend failures (`effective_config`/`retrieval_store`)
/// return `Err` here and abort daemon startup, matching
/// `build_daemon_state`'s existing fail-fast behavior for a broken backend.
/// A failure to construct the *embedder* is deliberately not one of those
/// cases — see [`UnavailableEmbedder`].
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
    let embed_policy = &yaml.defaults.indexing.embedding;
    // Server has no `models_dir` override the way the CLI does (see
    // `run_mcp_async`) — mirrors `search_service.rs`'s `create_embedder` call.
    let embedder: Arc<dyn Embedder> = match embed::create_embedder(
        embed_policy,
        &yaml.providers,
        None,
    ) {
        Ok(e) => Arc::from(e),
        Err(e) => {
            // The default `local` provider's model may simply not be
            // downloaded yet on a fresh daemon — `/v1/search` already
            // tolerates this by deferring `create_embedder` to each
            // request rather than daemon startup (`search_service.rs`).
            // Do the same here: start the daemon and mount `/mcp` with
            // real store handles, degrading only the `search` tool
            // (and only once actually invoked — see
            // `UnavailableEmbedder::embed_documents`) rather than
            // refusing to start the whole daemon over it.
            tracing::warn!(
                    "MCP: default embedder unavailable at startup ({e}); the /mcp `search` tool \
                     will return a tool-level error until this is resolved and the daemon is restarted"
                );
            Arc::new(UnavailableEmbedder {
                reason: e.to_string(),
            })
        }
    };

    Ok((stores, embedder))
}

/// Stand-in `Embedder` used when the real one couldn't be constructed at
/// daemon startup. Store handles are unaffected by this — only `search`
/// degrades, and only when a caller actually invokes it, at which point
/// `SearchOrchestrator::query`'s existing error handling turns this into a
/// normal tool-level `CallToolResult` error (see `tools::tool_search`),
/// not a panic or a refusal to start the daemon.
struct UnavailableEmbedder {
    reason: String,
}

#[async_trait::async_trait]
impl Embedder for UnavailableEmbedder {
    async fn embed_documents(
        &self,
        _docs: Vec<DocumentChunks>,
    ) -> Result<Vec<EmbeddedDocument>, Error> {
        Err(Error::ModelMissing {
            message: self.reason.clone(),
        })
    }

    fn embedding_dim(&self) -> usize {
        0
    }

    fn model_id(&self) -> &str {
        "unavailable"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job_queue::JobQueue;
    use crate::scheduler::UrlRefreshScheduler;
    use localdb_core::config::schema::{EmbeddingPolicy, RawConfig};

    async fn make_state(yaml_config: RawConfig) -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let queue = JobQueue::new();
        let state = AppState::new(
            yaml_config,
            dir.path().to_path_buf(),
            queue.clone(),
            UrlRefreshScheduler::new(queue),
        )
        .await
        .unwrap();
        (dir, state)
    }

    #[tokio::test]
    async fn degrades_to_unavailable_embedder_when_default_provider_unavailable() {
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

        let (stores, embedder) = build_available_stores(&state).await.unwrap();

        assert!(stores.is_empty());
        assert_eq!(embedder.model_id(), "unavailable");
        assert_eq!(embedder.embedding_dim(), 0);
        let err = embedder.embed_documents(vec![]).await.unwrap_err();
        assert!(
            matches!(err, Error::ModelMissing { .. }),
            "expected ModelMissing, got: {err:?}"
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

        let (stores, embedder) = build_available_stores(&state).await.unwrap();

        assert_eq!(stores.len(), 1);
        assert_eq!(stores[0].descriptor.name, "notes");
        assert_ne!(embedder.model_id(), "unavailable");
    }
}
