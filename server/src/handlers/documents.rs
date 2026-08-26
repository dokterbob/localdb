use axum::{
    extract::{Path, State},
    Json,
};
use axum_extra::extract::Query;
use serde::{Deserialize, Serialize};

use localdb_core::metadata::Metadata;
use localdb_core::DocumentInfo;

use super::{default_limit, parse_cursor, parse_limit, PaginatedList};
use crate::error::ApiError;
use crate::state::AppState;

/// Document record returned by the API.
#[derive(Debug, Clone, Serialize)]
pub struct DocumentRecord {
    pub id: String,
    pub uri: String,
    pub title: Option<String>,
    pub store_id: String,
    pub source_id: String,
    pub content_hash: String,
    pub fetched_at: String,
    pub normalized_text: String,
    pub metadata: Metadata,
    pub date_original: Option<String>,
    pub date_parsed: Option<String>,
    pub index_updated_at: Option<String>,
}

/// `GET /v1/documents/{id}` query params: a repeatable `?store=` scopes the
/// lookup to specific stores, same idiom as `?store=` on `GET /v1/status`
/// (`server/src/handlers/status.rs`) — `Vec<String>` + `#[serde(default)]`
/// via `axum_extra::extract::Query`, which correctly handles zero, one, or
/// many repeated params of the same name.
#[derive(Debug, Deserialize)]
pub struct GetDocumentQuery {
    #[serde(default)]
    pub store: Vec<String>,
}

pub async fn get_document(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
    Query(query): Query<GetDocumentQuery>,
) -> Result<Json<DocumentRecord>, ApiError> {
    let detail = state.get_document(&doc_id, &query.store).await?;
    let info = detail.info;
    Ok(Json(DocumentRecord {
        id: info.id,
        uri: info.uri,
        title: info.title,
        store_id: info.store_id,
        source_id: info.source_id,
        content_hash: info.content_hash,
        fetched_at: info.fetched_at,
        normalized_text: detail.text.unwrap_or_default(),
        metadata: info.metadata,
        date_original: info.date_original,
        date_parsed: info.date_parsed,
        index_updated_at: info.index_updated_at,
    }))
}

/// `GET /v1/stores/{name}/documents` query params: cursor/limit pagination
/// (same idiom as `GET /v1/stores/{name}/sources`'s `PaginationParams`) plus
/// an optional `?source=` filter. A dedicated struct rather than reusing
/// `PaginationParams` directly — `source` isn't a pagination concern, and
/// flattening two `Deserialize` structs together doesn't play well with
/// `serde_urlencoded` (the wire format `axum::extract::Query` parses).
#[derive(Debug, Deserialize)]
pub struct ListDocumentsQuery {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

pub async fn list_documents(
    State(state): State<AppState>,
    Path(store_name): Path<String>,
    axum::extract::Query(query): axum::extract::Query<ListDocumentsQuery>,
) -> Result<Json<PaginatedList<DocumentInfo>>, ApiError> {
    let offset = parse_cursor(query.cursor.as_deref())?;
    let limit = parse_limit(query.limit)?;

    let (page, total) = state
        .list_documents(&store_name, query.source.as_deref(), Some(limit), offset)
        .await?;
    Ok(Json(PaginatedList::new(
        page,
        offset,
        limit,
        total as usize,
    )))
}
