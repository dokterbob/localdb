//! `StoreProvider` — the seam that makes MCP store resolution realtime.
//!
//! Design decision D12: `McpHandler` no longer holds a `Vec<AvailableStore>`
//! snapshot taken once at construction time. Instead it holds an
//! `Arc<dyn StoreProvider>` and calls [`StoreProvider::available_stores`]
//! fresh, at the top of every tool invocation. A store created after the
//! MCP service/handler was constructed (e.g. via `POST /v1/stores` on a
//! running daemon) is therefore visible on the very next tool call — no
//! restart needed. This was previously blocked by the mistaken belief that
//! rmcp's *synchronous* HTTP service-factory closure (`Fn() -> Result<S,
//! io::Error>`, see `http.rs`) prevented any async re-resolution; in fact
//! that closure only constrains *construction-time* lookups (building a new
//! `McpHandler` per session) — it says nothing about what an individual
//! `async fn` tool method does once the handler exists, which is exactly
//! where `available_stores().await` is now called.
//!
//! A later ticket (T5) will filter a provider's result by an authenticated
//! `Principal` per call; this trait's single async method — no request
//! context threaded through it yet — is deliberately left room for that
//! (e.g. a decorator provider wrapping another `Arc<dyn StoreProvider>`)
//! without needing a signature change here.

use async_trait::async_trait;

use localdb_core::Error;

use crate::tools::AvailableStore;

/// Resolves the set of stores visible to an MCP session, fresh on every call.
#[async_trait]
pub trait StoreProvider: Send + Sync {
    /// Return the currently available stores.
    ///
    /// Called at the top of every tool method (see `handler.rs`) — anything
    /// this does is on the hot path of every `search`/`get_document`/
    /// `get_chunks`/`list_stores` call, so implementations should keep it
    /// cheap (a DB list-query plus per-store handle construction, not a full
    /// re-scan of content).
    async fn available_stores(&self) -> Result<Vec<AvailableStore>, Error>;
}

/// A `StoreProvider` over a fixed, already-resolved list of stores.
///
/// Used by tests (which build their `AvailableStore`s directly) and any
/// legacy call site that has a `Vec<AvailableStore>` in hand and doesn't
/// need per-call re-resolution — e.g. a one-shot CLI invocation with no
/// concept of "later".
#[derive(Clone)]
pub struct StaticStoreProvider {
    stores: Vec<AvailableStore>,
}

impl StaticStoreProvider {
    /// Wrap a fixed `Vec<AvailableStore>`.
    pub fn new(stores: Vec<AvailableStore>) -> Self {
        Self { stores }
    }
}

#[async_trait]
impl StoreProvider for StaticStoreProvider {
    async fn available_stores(&self) -> Result<Vec<AvailableStore>, Error> {
        Ok(self.stores.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn static_provider_returns_its_fixed_stores() {
        let provider = StaticStoreProvider::new(vec![]);
        let stores = provider.available_stores().await.unwrap();
        assert!(stores.is_empty());
    }
}
