use serde::{Deserialize, Serialize};

use crate::block::{IngestorKind, Resource};
use crate::error::Error;
use crate::uri::Uri;

/// Configuration field descriptor for an ingestor's setup.
///
/// Used by CLI to generate interactive prompts for ingestor configuration.
/// See specs/03-config.md §3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigField {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub required: bool,
    /// Secret fields are stored in the credentials table, not config_json.
    pub secret: bool,
    pub field_type: ConfigFieldType,
    pub default: Option<String>,
}

/// Type of a configuration field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFieldType {
    String,
    Path,
    Url,
    Integer,
    Boolean,
    Choice(Vec<String>),
}

/// Trait for ingestor-specific configuration.
///
/// Each ingestor kind has a corresponding config type that describes its
/// required and optional fields. This enables the CLI to generate interactive
/// setup prompts and the HTTP API to validate source configurations.
///
/// Lives in `core` as part of the contract. Concrete implementations live
/// outside `core`.
pub trait IngestorConfig: Send + Sync {
    /// Describe the configuration fields for this ingestor.
    fn fields(&self) -> Vec<ConfigField>;

    /// Validate a JSON config object against this ingestor's requirements.
    fn validate(&self, config: &serde_json::Value) -> Result<(), Error>;
}

/// The Ingestor trait — contract for content acquisition and structuring.
///
/// Each ingestor knows how to connect to a source, enumerate content, and
/// produce `Resource`s with typed blocks. The trait yields an async stream
/// of resources.
///
/// Lives in `core` as the contract. Concrete ingestor implementations (file,
/// URL, Notion, Telegram, etc.) live outside `core`, consistent with the
/// "no I/O frameworks in core" invariant.
///
/// See specs/02-domain-model.md §8 and specs/01-architecture.md §1.
#[async_trait::async_trait]
pub trait Ingestor: Send + Sync {
    /// Which ingestor kind this is.
    fn kind(&self) -> IngestorKind;

    /// Ingest content from a source, yielding resources.
    ///
    /// The implementation should:
    /// - Connect to the source (file scan, HTTP fetch, API call)
    /// - Enumerate content items
    /// - Produce a `Resource` with typed blocks for each item
    /// - Yield resources as they become available
    ///
    /// Resources are yielded via callback rather than returned as a Vec to
    /// support streaming large sources without buffering all resources in
    /// memory. The callback receives each resource as it's produced.
    async fn ingest(
        &self,
        source: &IngestSource,
        callback: &mut dyn IngestCallback,
    ) -> Result<IngestResult, Error>;
}

/// Why an ingestor skipped a discovered item without producing a `Resource`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Content unchanged since the last indexed run (hash/mtime/etag match
    /// under the same policy version).
    Unchanged,
    /// No configured parser supports the item's format.
    Unsupported,
    /// Skipped for another reason (benign, non-error skip: filtered by
    /// configuration, ...). The string is a human-readable explanation. Not
    /// for I/O or parser errors — use [`SkipReason::Error`] for those.
    Other(String),
    /// Processing failed but the item still exists — keeps the URI alive in
    /// the delete-sweep (it's a transient failure, not evidence the resource
    /// is gone) while still being counted as an error rather than a benign
    /// skip (issue C7/C8: previously such failures were folded into
    /// `Other`, which under-reported errors as skips). The string is a
    /// human-readable explanation (read error, parser error, parser panic).
    Error(String),
}

/// Callback for receiving resources during ingestion.
///
/// This is the streaming interface: the ingestor calls `on_resource` for each
/// resource it produces, and the caller processes it immediately. The
/// `on_discovered` / `on_skipped` hooks are optional progress signals with
/// default no-op implementations so simple ingestors and test callbacks can
/// ignore them.
#[async_trait::async_trait]
pub trait IngestCallback: Send {
    async fn on_resource(&mut self, resource: Resource) -> Result<(), Error>;

    /// Called once the ingestor knows how many items it will consider
    /// (after enumeration). Streaming ingestors that never know a total
    /// simply never call this.
    async fn on_discovered(&mut self, _total: usize) {}

    /// Called for each discovered item the ingestor decides not to turn into
    /// a `Resource` (unchanged content, unsupported format, ...).
    ///
    /// `core` owns identity/normalization for every locator that flows
    /// through the pipeline (see `core::ingestion::normalize_uri` and
    /// `Uri`'s own construction guarantees) — a `Uri` reaching this method is
    /// already canonical (percent-encoded path bytes, lower-cased host,
    /// etc.), the same representation `Resource.uri` carries on the
    /// `on_resource` path. An ingestor is never required to normalize a
    /// locator itself to stay correct: it only has to produce a valid `Uri`
    /// in the first place (typically by building one with `Uri::parse` /
    /// `Uri::from_file_path` up front and reusing it), never a raw string
    /// that core then has to reconcile against its own bookkeeping.
    async fn on_skipped(&mut self, _uri: &Uri, _reason: SkipReason) {}

    /// Called when the ingestor has *positively established* that a previously
    /// indexed locator no longer exists at the origin — an HTTP 404/410
    /// confirmed after retry, an API that answers "deleted" for an id.
    ///
    /// This is the counterpart to the delete-sweep, and the distinction
    /// between the two is the whole of issues #156/#185: **knowing a resource
    /// is gone is not the same as failing to find it.** A 410 is knowledge —
    /// the origin was reached and it answered. A file missing from a directory
    /// walk is merely an absence, and an absence is only informative if the
    /// walk itself was trustworthy (see [`Enumeration`]).
    ///
    /// So a URI reported here is deleted unconditionally: no sweep guard
    /// applies to it, because no guard needs to — nothing was inferred. An
    /// ingestor that merely fails to observe a locator must NOT call this;
    /// staying silent and letting the guarded sweep decide is correct there.
    async fn on_gone(&mut self, _uri: &Uri) {}
}

/// Source information passed to an ingestor.
#[derive(Debug, Clone)]
pub struct IngestSource {
    pub source_id: String,
    pub store_id: String,
    pub ingestor_kind: IngestorKind,
    pub config: serde_json::Value,
    /// Hash of the indexing policy in effect for this run. Ingestors stamp it
    /// into produced `Resource`s and may use it for incremental-skip checks
    /// (a policy change invalidates previously indexed content).
    pub policy_version: String,
}

/// Whether an ingestion run saw the source's *complete* current contents.
///
/// The distinction this type exists to force is the one behind issue #156:
/// "I observed nothing" is not "it was deleted." An ingestor that could not
/// reach its source at all (unmounted volume, unreachable root, an API that
/// failed mid-enumeration) has produced no evidence about what still exists —
/// and `run_source_ingestion`'s delete-sweep, which infers deletion from
/// absence, must not run on that basis. Only `Complete` licenses the sweep.
///
/// Note the asymmetry with an *error*: an ingestor that returns `Err` already
/// aborts before the sweep. `Incomplete` is for the case where the run
/// otherwise succeeded — the ingestor has partial or no observations to report
/// but nothing failed hard enough to fail the run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Enumeration {
    /// The ingestor enumerated the source's full current contents: a URI it
    /// did not report this run really is gone.
    #[default]
    Complete,
    /// The ingestor could not observe the full source. `reason` is a
    /// human-readable explanation, surfaced in the warning that reports the
    /// suppressed sweep.
    Incomplete { reason: String },
}

/// Result of an ingestion run.
#[derive(Debug, Clone, Default)]
pub struct IngestResult {
    pub resources_produced: usize,
    pub resources_skipped: usize,
    pub errors: usize,
    /// Whether this run observed the source's complete contents. Defaults to
    /// [`Enumeration::Complete`], so an ingestor that always enumerates
    /// exhaustively (`UrlIngestor`, `FeedIngestor`) needs no change.
    pub enumeration: Enumeration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_field_type_roundtrip() {
        let types = vec![
            ConfigFieldType::String,
            ConfigFieldType::Path,
            ConfigFieldType::Url,
            ConfigFieldType::Integer,
            ConfigFieldType::Boolean,
            ConfigFieldType::Choice(vec!["a".to_string(), "b".to_string()]),
        ];
        for t in &types {
            let json = serde_json::to_string(t).unwrap();
            let t2: ConfigFieldType = serde_json::from_str(&json).unwrap();
            assert_eq!(t, &t2);
        }
    }

    #[test]
    fn config_field_roundtrip() {
        let field = ConfigField {
            key: "api_token",
            label: "API Token",
            description: "Your Notion integration token",
            required: true,
            secret: true,
            field_type: ConfigFieldType::String,
            default: None,
        };
        let json = serde_json::to_string(&field).unwrap();
        assert!(json.contains("api_token"));
        // ConfigField has &'static str so can't deserialize from runtime string,
        // but serialization proves the shape is correct.
    }

    #[test]
    fn ingest_source_creation() {
        let source = IngestSource {
            policy_version: "test-policy".to_string(),
            source_id: "src-1".to_string(),
            store_id: "store-1".to_string(),
            ingestor_kind: IngestorKind::File,
            config: serde_json::json!({ "root": "/tmp/docs" }),
        };
        assert_eq!(source.ingestor_kind, IngestorKind::File);
    }

    #[test]
    fn skip_reason_error_is_distinct_from_other() {
        // C8: `Error` and `Other` must remain distinct variants so callers
        // (PipelineCallback::on_skipped) can bucket them differently —
        // `Other` as a benign skip, `Error` as a counted error.
        let err = SkipReason::Error("boom".to_string());
        let other = SkipReason::Other("boom".to_string());
        assert_ne!(err, other);
        assert_eq!(err, SkipReason::Error("boom".to_string()));
    }

    #[test]
    fn ingest_result_default() {
        let result = IngestResult::default();
        assert_eq!(result.resources_produced, 0);
        assert_eq!(result.resources_skipped, 0);
        assert_eq!(result.errors, 0);
        // #156: an ingestor that says nothing about enumeration completeness
        // is claiming a complete view — the sweep-licensing default.
        assert_eq!(result.enumeration, Enumeration::Complete);
    }
}
