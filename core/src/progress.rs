/// Progress events emitted during ingestion.
///
/// Shaped for parallel readiness: per-doc events are keyed by URI so
/// out-of-order completion renders correctly, and `Discovered` is separate
/// from per-doc events so a streaming walk can emit incremental discovery.
///
/// `Serialize`/`Deserialize` are derived so this type can cross a wire
/// boundary (issue #83: SSE live job progress over `GET
/// /v1/jobs/{id}/events`). Internally tagged (`tag = "type"`) to match this
/// codebase's other wire enums (e.g. `IndexJobScope`) and to keep the JSON
/// shape flat and easy for a JS `EventSource` consumer to switch on.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProgressEvent {
    SourceStarted {
        source_id: String,
        location: String,
    },
    /// File count is known (after enumeration).
    Discovered {
        total: usize,
    },
    DocumentStarted {
        uri: String,
        index: usize,
        total: usize,
    },
    DocumentFinished {
        uri: String,
        outcome: DocOutcome,
    },
    SourceFinished {
        result: crate::ingestion::IngestionResult,
    },
}

/// Outcome of processing a single document.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum DocOutcome {
    Indexed {
        chunks: usize,
    },
    Skipped,
    Unsupported,
    Error,
    /// Content and policy were unchanged, but persisted metadata (Dublin
    /// Core fields, `external_id`/`external_etag`/`modified_at`) differed
    /// from what's stored — the resource row was rewritten in place, no
    /// chunks/embeddings touched (issue #176; specs/04-search-pipeline.md).
    MetadataUpdated,
}

/// A cheaply-cloneable progress callback, `Send + Sync` for future parallel use.
pub type ProgressSink = std::sync::Arc<dyn Fn(ProgressEvent) + Send + Sync>;
