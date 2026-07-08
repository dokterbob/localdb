use axum::{
    extract::{Path, State},
    Extension, Json,
};
use serde::Serialize;

use localdb_core::auth::Principal;
use localdb_core::parser::DocumentMetadata;
use localdb_core::Error as CoreError;

use super::require_principal;
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

/// `GET /v1/documents/{id}`: readable like its owning store (D7). A
/// document in a store the caller cannot read is masked as
/// `document_not_found` when the *document itself* is unknown, but as
/// `forbidden` (403) when it exists in a store the caller cannot read —
/// same 403-over-404 consistency point as `handlers::stores::get_store`.
pub async fn get_document(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(doc_id): Path<String>,
) -> Result<Json<DocumentRecord>, ApiError> {
    let principal = require_principal(principal)?;
    let info = state
        .backend()
        .find_document(&doc_id)
        .await
        .map_err(ApiError)?
        .ok_or(ApiError(CoreError::DocumentNotFound { id: doc_id.clone() }))?;

    // A dangling `store_id` (no owning store row) is not expected in
    // practice — cascade deletes remove documents with their store — but if
    // it ever happens, fail open to serving the document rather than
    // panicking: there is no visibility to check against.
    if let Some(store) = state
        .backend()
        .get_store(&info.store_id)
        .await
        .map_err(ApiError)?
    {
        if !principal.can_read_store(&store.name, store.visibility.clone()) {
            return Err(ApiError(CoreError::Forbidden {
                message: format!("user '{}' cannot read document '{doc_id}'", principal.name),
            }));
        }
    }

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
