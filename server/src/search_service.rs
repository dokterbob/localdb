use serde::{Deserialize, Serialize};

use localdb_core::{
    clamp_search_limit, Citation, Error as CoreError, QueryRequest, SearchFilters,
    SearchOrchestrator, StoreHandle as CoreStoreHandle,
};

use crate::error::ApiError;
use crate::handlers::parse_cursor;
use crate::state::AppState;

/// `POST /v1/search` request body, and — serialized rather than
/// `serde_json::json!`-hand-built — the CLI's daemon-attached POST body too
/// (`cli::cmds::search::SearchCmd::run_daemon`, issue #247): one shared type
/// deserialized/serialized on both ends means the two can never drift apart
/// on field names the way a hand-built `json!` body could.
#[derive(Debug, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub store_filter: Vec<String>,
    #[serde(default = "default_search_limit")]
    pub limit: usize,
    #[serde(default)]
    pub cursor: Option<String>,
    /// Search-scoping filters (path, mime, date-axis bounds) — flattened so
    /// their fields (`path`, `mime`, `added_after`, …) sit at the top level
    /// of the request body rather than nested under a `filters` key.
    #[serde(flatten)]
    pub filters: SearchFilters,
}

fn default_search_limit() -> usize {
    10
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub citations: Vec<Citation>,
    pub total_candidates: usize,
    pub next_cursor: Option<String>,
}

pub struct SearchService {
    state: AppState,
}

impl SearchService {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }

    pub async fn query(&self, req: SearchRequest) -> Result<SearchResponse, ApiError> {
        if req.query.is_empty() {
            return Err(ApiError(CoreError::InvalidRequest {
                message: "query cannot be empty".to_string(),
            }));
        }

        // Validate/convert filters up front — a malformed date bound is a
        // 400, not a 500 or a silently-empty result set (issue #247).
        let metadata_filters = req.filters.into_metadata_filters().map_err(ApiError)?;

        let offset = parse_cursor(req.cursor.as_deref())?;
        let limit = clamp_search_limit(req.limit);
        let page_end = resolve_page_end(offset, limit)?;

        let effective = self.state.effective_config().await?;
        for name in &req.store_filter {
            if !effective.stores.iter().any(|s| s.name == *name) {
                return Err(ApiError(CoreError::StoreNotFound { id: name.clone() }));
            }
        }

        let yaml = self.state.yaml_config().await;
        let embed_policy = &yaml.defaults.indexing.embedding;

        let embedder: Box<dyn localdb_core::Embedder> = embed::create_embedder(
            embed_policy,
            &yaml.providers,
            Some(self.state.models_dir()),
            &(&yaml.http).into(),
        )
        .map_err(|e| {
            ApiError(CoreError::InvalidConfig {
                message: e.to_string(),
            })
        })?;

        let target_stores: Vec<_> = if req.store_filter.is_empty() {
            effective.stores.iter().collect()
        } else {
            effective
                .stores
                .iter()
                .filter(|s| req.store_filter.contains(&s.name))
                .collect()
        };

        let mut store_handles: Vec<CoreStoreHandle> = Vec::new();

        for store_cfg in target_stores {
            let store_id = store_cfg.id.clone();
            let handle = self
                .state
                .backend()
                .retrieval_store(&store_id)
                .await
                .map_err(ApiError)?;
            store_handles.push(CoreStoreHandle {
                id: store_id,
                name: store_cfg.name.clone(),
                store: handle,
            });
        }

        if store_handles.is_empty() {
            return Ok(SearchResponse {
                citations: vec![],
                total_candidates: 0,
                next_cursor: None,
            });
        }

        let query_request = QueryRequest {
            query: req.query.clone(),
            leg_k: None,
            top_n: Some(page_end),
            filters: metadata_filters,
        };

        let response = SearchOrchestrator::query(&store_handles, embedder.as_ref(), &query_request)
            .await
            .map_err(ApiError)?;

        let total = response.total_candidates;
        let next_cursor = if page_end < total {
            Some(format!("{page_end}"))
        } else {
            None
        };

        let citations = response.citations.into_iter().skip(offset).collect();

        Ok(SearchResponse {
            citations,
            total_candidates: total,
            next_cursor,
        })
    }
}

/// Resolve the exclusive end of the requested page (`offset + limit`) as a
/// single checked computation, reused for `top_n`, the `next_cursor`
/// comparison, and the `next_cursor` value (issue #187 review, finding G3) —
/// previously this addition was performed three times, unchecked, and a
/// client-supplied `cursor` near `usize::MAX` paired with any `limit` would
/// overflow: panicking in debug (no `CatchPanicLayer`, so the connection
/// just dies) or silently wrapping in release (an empty page plus a bogus
/// `next_cursor`). A page end that cannot be represented as a `usize` is
/// rejected as `invalid_request`, HTTP 400, rather than either of those.
fn resolve_page_end(offset: usize, limit: usize) -> Result<usize, ApiError> {
    offset.checked_add(limit).ok_or_else(|| {
        ApiError(CoreError::InvalidRequest {
            message: format!(
                "pagination cursor '{offset}' combined with limit '{limit}' overflows; \
                 request an earlier page or a smaller limit"
            ),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // `clamp_search_limit`/`SEARCH_MAX_LIMIT` are re-exported from
    // `localdb_core::search` (issue #187 review) — their own coverage lives
    // in `localdb_core::search::tests::clamp_search_limit_*` rather than
    // being duplicated here.

    #[test]
    fn resolve_page_end_adds_offset_and_limit() {
        assert_eq!(resolve_page_end(3, 7).unwrap(), 10);
        assert_eq!(resolve_page_end(0, 0).unwrap(), 0);
    }

    #[test]
    fn resolve_page_end_rejects_overflow_as_invalid_request() {
        let err = resolve_page_end(usize::MAX, 1).expect_err("overflow must be rejected");
        match err.0 {
            CoreError::InvalidRequest { .. } => {}
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }
}
