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
    /// No new content, but the resource row *was* rewritten in place: a 304
    /// carrying a rotated validator, or a connector re-supplying metadata
    /// that moved on ([`MetadataWriteOutcome::Written`]).
    ///
    /// Distinct from [`Self::Unchanged`] because it is not a skip. It counts
    /// toward `docs_metadata_updated`, exactly as the same write does when it
    /// arrives through `on_resource`'s metadata-only branch — a URI counted
    /// as both a skip and a metadata update would break the invariant that
    /// the outcome counters partition `docs_seen`
    /// (specs/04-search-pipeline.md).
    MetadataUpdated,
    /// The feed entry recheck gate found this entry already known at the
    /// run's policy, inside the recheck floor, with an unchanged feed claim
    /// — so no HTTP request was made for it at all
    /// (specs/04-search-pipeline.md §1 "Recheck gate").
    ///
    /// Distinct from [`Self::Unchanged`] because no origin contact happened:
    /// unlike an actual 304, this must **never** advance `last_checked_at` —
    /// doing so would slide the recheck floor forward on every gated run and
    /// the entry would never be re-verified again. Counts toward
    /// `docs_skipped`, same as [`Self::Unchanged`], plus the dedicated
    /// `IngestionResult::docs_recheck_deferred` sub-counter.
    Fresh,
}

/// What a metadata-refresh hook did to the store.
///
/// The two refresh hooks — [`IngestCallback::on_validators_refreshed`] and
/// [`IngestCallback::on_metadata_refreshed`] — both run behind a 304, both
/// may rewrite the resource row, and both may fail. Returning nothing left
/// the caller reporting every 304 as a plain skip: a write that happened went
/// uncounted, and a write that *failed* was reported as a clean skip, so the
/// run's error count stayed zero while the metadata staleness persisted.
///
/// The caller folds the two outcomes with [`Self::merge`] and reports the URI
/// exactly once, so the seen-set and the progress stream each see one event
/// per URI regardless of how many hooks wrote.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MetadataWriteOutcome {
    /// Nothing needed writing: the incoming state matched what is stored.
    /// Also the default for the trait's no-op implementations.
    #[default]
    Unchanged,
    /// The resource row was rewritten in place.
    Written,
    /// The write was attempted and failed. The string is a human-readable
    /// explanation, carried through to `SkipReason::Error`.
    Failed(String),
}

impl MetadataWriteOutcome {
    /// Fold the outcomes of two hooks over one URI into the single outcome
    /// its caller reports, by severity: `Failed` outranks `Written`, which
    /// outranks `Unchanged`.
    ///
    /// `Failed` wins because a run that failed a write must report an error
    /// even when the other hook succeeded — the resource is left in a state
    /// neither hook intended, and the next run has to retry. Between two
    /// failures the first is kept; both name the same resource, and the
    /// second's message adds nothing the first does not already surface.
    pub fn merge(self, other: Self) -> Self {
        match (self, other) {
            (f @ Self::Failed(_), _) => f,
            (_, f @ Self::Failed(_)) => f,
            (Self::Written, _) | (_, Self::Written) => Self::Written,
            _ => Self::Unchanged,
        }
    }
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

    /// Look up conditional-GET validators stored from a previous successful
    /// fetch of `uri`, to replay as `If-None-Match`/`If-Modified-Since` on
    /// this fetch (`url` sources and feed entry links — see
    /// `specs/04-search-pipeline.md` §1). The default empty `FetchMetadata`
    /// means "no previous validators known," matching this trait's other
    /// default-no-op methods so ingestors and test callbacks that don't need
    /// replay can ignore it.
    ///
    /// `&mut self`, matching every other method on this trait, even though
    /// this one is a pure lookup with nothing to record. A plain `&self`
    /// looks like the better fit, and compiles standalone, but not through
    /// `#[async_trait]`: a `&self` method desugars to a boxed future that
    /// must be `Send`, which requires `&Self: Send`, which requires
    /// `Self: Sync` — a bound this trait doesn't otherwise carry (its
    /// `&mut self` methods only need `Self: Send`) and that every
    /// implementor holding a `&mut DocumentIndex`-style field would have to
    /// start satisfying too. `&mut self` avoids widening the trait's bounds
    /// for one method's convenience; callers already hold
    /// `&mut dyn IngestCallback`, so this costs them nothing.
    async fn lookup_fetch_metadata(&mut self, _uri: &Uri) -> crate::ingestion::FetchMetadata {
        crate::ingestion::FetchMetadata::default()
    }

    /// Called when a 304 Not Modified response itself carried a refreshed
    /// validator (RFC 9111 requires storing one even though the body is
    /// unchanged — see `FetchResult::NotModified`'s doc comment). `meta`
    /// mirrors that variant's contract exactly: `None` in either field means
    /// "unchanged, leave the stored value alone," never "clear it." The
    /// default no-op matches every other optional signal on this trait.
    ///
    /// Returns what it did to the store, so the caller can report the URI as
    /// a metadata update or an error rather than a plain skip — see
    /// [`MetadataWriteOutcome`].
    async fn on_validators_refreshed(
        &mut self,
        _uri: &Uri,
        _meta: &crate::ingestion::FetchMetadata,
    ) -> MetadataWriteOutcome {
        MetadataWriteOutcome::Unchanged
    }

    /// Called when a connector re-supplies its own description of an
    /// already-indexed resource whose *body* did not change — a feed entry
    /// whose link answered 304 while the feed's own metadata for it moved
    /// on.
    ///
    /// Without this, a 304 would freeze connector-supplied metadata forever:
    /// the response carries no body, so there is nothing to re-parse and
    /// nothing to route through `on_resource`, and a feed that corrects an
    /// entry's author or publication date would never see the correction
    /// land. `enrichment` is the connector's claim; the implementor layers
    /// it onto the resource's *persisted* metadata via
    /// [`crate::metadata::MetadataEnrichment::apply_to`], which is the same
    /// merge the ingestor applies to freshly parsed metadata at index time —
    /// so the two paths cannot drift into disagreeing about what a feed's
    /// metadata means.
    ///
    /// `external_id` and `modified_at` are the connector's claims too, and
    /// are passed separately because they live on the `Resource` rather than
    /// inside `Metadata`. Both are authoritative: `None` means the connector
    /// makes no claim, exactly as it would at index time, not "leave the
    /// stored value alone."
    ///
    /// Deliberately separate from [`Self::on_validators_refreshed`] rather
    /// than folded into a wider signature: a plain URL fetch has no
    /// connector metadata at all and would otherwise pass empty claims on
    /// every 304 forever. The default no-op matches every other optional
    /// signal on this trait.
    ///
    /// Returns what it did to the store, on the same contract as
    /// [`Self::on_validators_refreshed`] — see [`MetadataWriteOutcome`].
    async fn on_metadata_refreshed(
        &mut self,
        _uri: &Uri,
        _enrichment: &crate::metadata::MetadataEnrichment,
        _external_id: Option<&str>,
        _modified_at: Option<&str>,
    ) -> MetadataWriteOutcome {
        MetadataWriteOutcome::Unchanged
    }
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
    /// Conditional-GET validators stored for the source's own top-level
    /// document (`sources.feed_etag`/`feed_last_modified`), to replay on the
    /// next fetch of that document. Consulted only by [`crate::block::IngestorKind::Feed`]
    /// — the feed document itself, not an entry's linked page, which has its
    /// own per-resource validators reached through
    /// [`IngestCallback::lookup_fetch_metadata`] instead. Other ingestor
    /// kinds ignore this field. Empty (`FetchMetadata::default()`) means "no
    /// prior validators known", identical to a first-ever fetch.
    pub document_validators: crate::ingestion::FetchMetadata,
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
    /// Refreshed validators for the source's own top-level document, to
    /// persist onto `sources.feed_etag`/`feed_last_modified` — the mirror
    /// image of [`IngestSource::document_validators`]. `None` means "leave
    /// whatever is stored alone" (no document fetch happened, or a bare 304
    /// carried no rotated validator); `Some` — even with both fields `None`
    /// inside — means "replace the stored validators with this," which is
    /// how a fresh 200 that dropped a previously-sent header clears it.
    /// Populated only by [`crate::block::IngestorKind::Feed`].
    pub document_validators: Option<crate::ingestion::FetchMetadata>,
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
            document_validators: crate::ingestion::FetchMetadata::default(),
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
        assert_eq!(result.document_validators, None);
    }

    /// A callback that overrides nothing but `on_resource` (the only
    /// non-defaulted method) must still get an empty `FetchMetadata` back
    /// from `lookup_fetch_metadata` — the conditional-GET replay seam is
    /// opt-in, like every other hook on this trait.
    #[tokio::test]
    async fn lookup_fetch_metadata_default_is_empty() {
        struct NoopCallback;
        #[async_trait::async_trait]
        impl IngestCallback for NoopCallback {
            async fn on_resource(&mut self, _resource: Resource) -> Result<(), Error> {
                Ok(())
            }
        }
        let mut cb = NoopCallback;
        let uri = Uri::parse("https://example.com/doc").unwrap();
        let meta = cb.lookup_fetch_metadata(&uri).await;
        assert_eq!(meta.etag, None);
        assert_eq!(meta.last_modified, None);
    }
}
