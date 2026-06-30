use serde::{Deserialize, Serialize};

use localdb_core::{
    Citation, Error as CoreError, QueryRequest, SearchOrchestrator, StoreHandle as CoreStoreHandle,
};

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub store_filter: Vec<String>,
    #[serde(default = "default_search_limit")]
    pub limit: usize,
    #[serde(default)]
    pub cursor: Option<String>,
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

        let offset = parse_cursor(req.cursor.as_deref())?;

        let effective = self.state.effective_config().await?;
        for name in &req.store_filter {
            if !effective.stores.iter().any(|s| s.name == *name) {
                return Err(ApiError(CoreError::StoreNotFound { id: name.clone() }));
            }
        }

        let yaml = self.state.yaml_config().await;
        let embed_policy = &yaml.defaults.indexing.embedding;

        let embedder: Box<dyn localdb_core::Embedder> =
            embed::create_embedder(embed_policy, &yaml.providers, None).map_err(|e| {
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
            top_n: Some(req.limit),
            filters: vec![],
        };

        let response = SearchOrchestrator::query(&store_handles, embedder.as_ref(), &query_request)
            .await
            .map_err(ApiError)?;

        let total = response.total_candidates;
        let next_cursor = if offset + req.limit < total {
            Some(format!("{}", offset + req.limit))
        } else {
            None
        };

        Ok(SearchResponse {
            citations: response.citations,
            total_candidates: total,
            next_cursor,
        })
    }
}

fn parse_cursor(cursor: Option<&str>) -> Result<usize, ApiError> {
    match cursor {
        None => Ok(0),
        Some(s) => s.parse::<usize>().map_err(|_| {
            ApiError(CoreError::InvalidRequest {
                message: format!(
                    "invalid pagination cursor '{s}'; expected a non-negative integer"
                ),
            })
        }),
    }
}
