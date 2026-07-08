use serde::{Deserialize, Serialize};

use localdb_core::{
    auth::Principal, types::StoreVisibility, Citation, Error as CoreError, QueryRequest,
    SearchOrchestrator, StoreHandle as CoreStoreHandle,
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

    /// D7 store scoping: if `req.store_filter` names specific stores, every
    /// one of them must exist *and* be readable by `principal` — an
    /// existing-but-unreadable named store is a 403 (the caller asked for it
    /// by name and was refused), matching `handlers::stores::get_store`'s
    /// consistency point. With no filter ("search all visible stores"), the
    /// full store set is silently narrowed to what `principal` can read —
    /// there is nothing to be "forbidden" from since nothing specific was
    /// requested.
    pub async fn query(
        &self,
        req: SearchRequest,
        principal: &Principal,
    ) -> Result<SearchResponse, ApiError> {
        if req.query.is_empty() {
            return Err(ApiError(CoreError::InvalidRequest {
                message: "query cannot be empty".to_string(),
            }));
        }

        let offset = parse_cursor(req.cursor.as_deref())?;

        let effective = self.state.effective_config().await?;
        for name in &req.store_filter {
            let Some(store) = effective.stores.iter().find(|s| s.name == *name) else {
                return Err(ApiError(CoreError::StoreNotFound { id: name.clone() }));
            };
            if !can_read(principal, &store.name, &store.visibility) {
                return Err(ApiError(CoreError::Forbidden {
                    message: format!("user '{}' cannot read store '{name}'", principal.name),
                }));
            }
        }

        let yaml = self.state.yaml_config();
        let embed_policy = &yaml.defaults.indexing.embedding;

        let embedder: Box<dyn localdb_core::Embedder> =
            embed::create_embedder(embed_policy, &yaml.providers, None).map_err(|e| {
                ApiError(CoreError::InvalidConfig {
                    message: e.to_string(),
                })
            })?;

        let target_stores: Vec<_> = if req.store_filter.is_empty() {
            effective
                .stores
                .iter()
                .filter(|s| can_read(principal, &s.name, &s.visibility))
                .collect()
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

/// D7 read check over an `EffectiveStore`'s string `visibility`, treating an
/// unrecognized value as `private` (deny by default) — see
/// `StoreVisibility::parse`'s doc comment.
fn can_read(principal: &Principal, name: &str, visibility: &str) -> bool {
    let visibility = StoreVisibility::parse(visibility).unwrap_or(StoreVisibility::Private);
    principal.can_read_store(name, visibility)
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
