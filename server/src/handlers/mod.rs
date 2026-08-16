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
//!   DELETE /jobs/:id              — cancel a queued or running job
//!   GET  /jobs/:id/events         — stream live job progress (SSE)
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
pub use jobs::{cancel_job, create_job, get_job, job_events};
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
        // `offset + limit` is unchecked-add territory (issue #187 review,
        // finding G3, sibling of the same bug in `search_service`): a
        // client-supplied `?cursor=` near `usize::MAX` combined with any
        // `?limit=` could otherwise panic in debug or wrap in release. Unlike
        // `/v1/search`'s `resolve_page_end`, an overflow here is never a
        // usable page — the offset already exceeds any real list length — so
        // it is treated as end-of-list (`next_cursor: None`) rather than
        // rejected; the caller already sliced `items` with `.skip(offset)`,
        // which degrades to empty instead of panicking, so silently reporting
        // "no more pages" is consistent with the data actually returned.
        let next_cursor = match offset.checked_add(limit) {
            Some(page_end) if page_end < total => Some(format!("{page_end}")),
            _ => None,
        };
        items.truncate(limit);
        Self {
            items,
            next_cursor,
            total,
        }
    }
}

#[cfg(test)]
mod paginated_list_tests {
    use super::PaginatedList;

    #[test]
    fn new_computes_next_cursor_for_a_normal_page() {
        let list = PaginatedList::new(vec!["a", "b"], 0, 2, 5);
        assert_eq!(list.next_cursor.as_deref(), Some("2"));
    }

    #[test]
    fn new_with_offset_and_limit_near_usize_max_does_not_panic_and_has_no_next_cursor() {
        // A client-supplied `?cursor=` near `usize::MAX` paired with any
        // `?limit=` must not panic on the unchecked `offset + limit` this
        // regresses (issue #187 review, finding G3, sibling of the
        // search_service bug). No real list is ever this long, so treating
        // the overflow as end-of-list is correct, not just safe.
        let list = PaginatedList::new(Vec::<&str>::new(), usize::MAX, 1, 5);
        assert_eq!(list.next_cursor, None);
        assert!(list.items.is_empty());

        let list = PaginatedList::new(Vec::<&str>::new(), 10, usize::MAX, 5);
        assert_eq!(list.next_cursor, None);
    }
}
