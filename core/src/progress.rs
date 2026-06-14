/// Progress events emitted during ingestion.
///
/// Shaped for parallel readiness: per-doc events are keyed by URI so
/// out-of-order completion renders correctly, and `Discovered` is separate
/// from per-doc events so a streaming walk can emit incremental discovery.
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
pub enum DocOutcome {
    Indexed { chunks: usize },
    Skipped,
    Unsupported,
    Error,
}

/// A cheaply-cloneable progress callback, `Send + Sync` for future parallel use.
pub type ProgressSink = std::sync::Arc<dyn Fn(ProgressEvent) + Send + Sync>;
