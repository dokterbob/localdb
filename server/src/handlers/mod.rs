//! Axum route handlers for the HTTP API.
//!
//! Every handler receives `State<AppState>` and returns a JSON response or
//! `ApiError`. The URL paths follow the resource list in specs/05-surfaces.md §3.
//!
//! Routes mounted at `/v1`:
//!   GET  /stores                  — list stores
//!   POST /stores                  — create runtime-owned store
//!   GET  /stores/:name            — get store by name
//!   PATCH /stores/:name           — update runtime-owned store
//!   DELETE /stores/:name          — delete runtime-owned store
//!   GET  /stores/:name/sources    — list sources for a store
//!   POST /stores/:name/sources    — add source to a store
//!   DELETE /sources/:id           — remove a source by ID
//!   GET  /documents/:id           — get document by ID
//!   POST /search                  — hybrid search
//!   POST /jobs                    — submit index job
//!   GET  /jobs/:id                — get job by ID
//!   GET  /status                  — daemon status
//!   GET  /config                  — resolved config

use serde::{Deserialize, Serialize};

use crate::error::ApiError;

mod config;
mod documents;
mod jobs;
mod search;
mod sources;
mod status;
mod stores;

pub use config::get_config;
pub use documents::get_document;
pub use jobs::{create_job, get_job};
pub use search::search;
pub use sources::{create_source, delete_source, list_sources};
pub use status::get_status;
pub use stores::{create_store, delete_store, get_store, list_stores, patch_store};

#[cfg(test)]
mod tests;

/// Cursor-based pagination parameters (from specs/05-surfaces.md §3).
#[derive(Debug, Deserialize)]
pub struct PaginationParams {
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    20
}

pub(crate) fn parse_cursor(cursor: Option<&str>) -> Result<usize, ApiError> {
    match cursor {
        None => Ok(0),
        Some(s) => s.parse::<usize>().map_err(|_| {
            ApiError(localdb_core::Error::InvalidRequest {
                message: format!(
                    "invalid pagination cursor '{s}'; expected a non-negative integer"
                ),
            })
        }),
    }
}

/// A paginated list response.
#[derive(Debug, Serialize)]
pub struct PaginatedList<T: Serialize> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    pub total: usize,
}

impl<T: Serialize> PaginatedList<T> {
    pub(crate) fn new(mut items: Vec<T>, offset: usize, limit: usize, total: usize) -> Self {
        let next_cursor = if offset + limit < total {
            Some(format!("{}", offset + limit))
        } else {
            None
        };
        items.truncate(limit);
        Self {
            items,
            next_cursor,
            total,
        }
    }
}
