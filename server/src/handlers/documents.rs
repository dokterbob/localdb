use axum::{
    extract::{Path, State},
    Json,
};
use serde::Serialize;

use localdb_core::parser::DocumentMetadata;
use localdb_core::Error as CoreError;

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
    pub metadata: DocumentMetadata,
}

pub async fn get_document(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
) -> Result<Json<DocumentRecord>, ApiError> {
    let info = state
        .backend()
        .find_document(&doc_id)
        .await
        .map_err(ApiError)?
        .ok_or(ApiError(CoreError::DocumentNotFound { id: doc_id.clone() }))?;
    let handle = state
        .backend()
        .retrieval_store(&info.store_id)
        .await
        .map_err(ApiError)?;
    let chunks = handle
        .get_chunks_for_document(&info.id)
        .await
        .map_err(ApiError)?;
    let normalized_text = chunks
        .iter()
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Json(DocumentRecord {
        id: info.id,
        uri: info.uri,
        title: info.title,
        store_id: info.store_id,
        source_id: info.source_id,
        content_hash: info.content_hash,
        fetched_at: info.fetched_at,
        normalized_text,
        metadata: info.metadata,
    }))
}
