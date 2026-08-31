//! Ingestion pipeline — scan-and-index orchestration.
//!
//! Coordinates: enumerate sources → acquire → extract → chunk → embed → upsert.
//!
//! Key behaviors:
//! - **Incremental skip**: if `content_hash` unchanged for a URI, skip reprocessing.
//! - **Replace-by-URI**: on change, delete old chunks then insert new ones.
//! - **Deletes**: file deleted / URL 404-410 / source removed → delete its chunks.
//! - **IndexJob lifecycle**: pending → running → done | failed; stats accumulated.
//! - **Policy version stamping**: every chunk carries `policy_version`; if the
//!   stored policy hash differs from the effective one, the store is marked stale.
//!
//! One-shot semantics only (T11 adds scheduling/watching).
//!
//! See specs/04-search-pipeline.md §1, §3, §4.
//!
//! Split across sibling modules by responsibility: [`deps`] is the
//! dependency-injection surface callers build; [`enumerate`] walks a `path`
//! source's filesystem; [`pipeline`] indexes one already-extracted `Resource`
//! (chunk → embed → upsert) and streams a source's resources into it; and
//! [`liveness`] probes aged-out feed entries. This file keeps the run-level
//! orchestration that ties them together — [`run_source_ingestion`] plus its
//! config/result/job-lifecycle types — so a reader who wants "what does one
//! ingestion run do end to end" never has to leave it.

use chrono::{DateTime, SecondsFormat, Utc};

use crate::chunker::ChunkerConfig;
use crate::error::Error;
use crate::ids::new_ulid;
use crate::ingestor::{Enumeration, IngestSource, Ingestor};
use crate::store::RetrievalStore;
use crate::types::{IndexJob, IndexJobScope, IndexJobState, IndexJobStats, Source, SourceSpec};

mod deps;
mod enumerate;
mod liveness;
mod pipeline;

#[cfg(any(test, feature = "test-support"))]
pub use deps::UnreachableFetcher;
pub use deps::{
    DeletionPolicy, DocumentIndex, DocumentRecord, FetchMetadata, FetchResult, IndexResourceDeps,
    SourceIngestionDeps, UrlFetcher,
};
pub use enumerate::{enumerate_path_source, FoundFile, PathEnumeration};
pub use pipeline::{index_resource, IndexOutcome};

use liveness::{feed_inputs_digest, run_feed_liveness_sweep};
use pipeline::PipelineCallback;

// ---------------------------------------------------------------------------
// IngestionConfig — parameters for a single pipeline run
// ---------------------------------------------------------------------------

/// Configuration for a single ingestion pipeline run.
#[derive(Clone)]
pub struct IngestionConfig {
    /// Store ID (ULID) owning this run.
    pub store_id: String,
    /// The computed policy version hash for the current indexing policy.
    pub policy_version: String,
    /// Chunking config derived from the effective store policy.
    pub chunker: ChunkerConfig,
}

// ---------------------------------------------------------------------------
// IngestionResult — summary returned by the pipeline after a run
// ---------------------------------------------------------------------------

/// Result of a completed ingestion pipeline run.
///
/// `Serialize`/`Deserialize` are derived because this type is embedded in
/// [`crate::progress::ProgressEvent::SourceFinished`], which crosses the
/// SSE wire boundary (issue #83).
///
/// `#[serde(default)]` at the **struct** level, matching
/// [`crate::types::IndexJobStats`], and for the same reason: this crosses a
/// version boundary. `localdb index` attaches to a running daemon's SSE
/// stream, and the two are not upgraded in lockstep — a newer CLI reading an
/// older daemon's `SourceFinished` frame would otherwise fail the whole
/// deserialize on the first field the daemon does not know about, dropping a
/// frame the user is watching rather than reading it with zeros for the
/// fields that are missing. Struct-level, not per-field, so every counter
/// added later inherits it without anyone having to remember.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct IngestionResult {
    /// Total documents seen in the scan.
    pub docs_seen: u64,
    /// Documents actually indexed (new or changed content).
    pub docs_indexed: u64,
    /// Documents skipped (unchanged content hash).
    pub docs_skipped: u64,
    /// Documents deleted (no longer in source).
    pub docs_deleted: u64,
    /// Total chunks written to the retrieval backend.
    pub chunks_written: u64,
    /// Files with unsupported format (counted but not errors).
    pub unsupported_format_count: u64,
    /// Files that errored during processing.
    pub error_count: u64,
    /// Documents this run would have deleted had deletion been enabled
    /// ([`DeletionPolicy::Prune`]) — either confirmed gone at the origin or
    /// absent from a trustworthy enumeration. Always 0 when pruning ran, since
    /// then they were actually deleted and counted in `docs_deleted`.
    ///
    /// Surfaced so a default (retaining) run can tell the user what `--delete`
    /// would remove, instead of silently accumulating stale documents.
    pub docs_prunable: u64,
    /// Documents whose content and policy were unchanged but whose metadata
    /// differed from what's persisted — the resource row was rewritten in
    /// place, no chunks/embeddings touched (issue #176). A strict subset of
    /// what would otherwise be counted in `docs_skipped`: a metadata-only
    /// update is not a content skip, but it is also not a full `docs_indexed`
    /// re-index, so it gets its own counter rather than overloading either.
    pub docs_metadata_updated: u64,
    /// Number of feed-discovered resources the liveness sweep probed this
    /// run — every candidate a fetch was actually attempted for, regardless
    /// of outcome (`Gone`, `NotModified`, `Downloaded`, `Blocked`, or a
    /// transport error all count; a candidate skipped because this run's
    /// own ingestion pass already observed it does not). Confirmed-gone
    /// prunes the sweep performs fold into `docs_deleted` above rather than
    /// a separate counter, so this is the only place the sweep's own probe
    /// work is visible — including on a run that deleted nothing. Always 0
    /// for a non-feed source and for any run under
    /// [`DeletionPolicy::Retain`] — see
    /// specs/04-search-pipeline.md §1 "Aged-out feed entries: the liveness
    /// sweep".
    pub feed_entries_liveness_checked: u64,
    /// Refreshed validators for a feed source's own top-level document, to
    /// persist onto `sources.feed_etag`/`feed_last_modified` — threaded
    /// straight from [`crate::ingestor::IngestResult::document_validators`].
    /// `None` for every non-feed source, and for a feed source whose run
    /// left the stored validators untouched (a bare 304, or no document
    /// fetch at all).
    pub document_validators: Option<FetchMetadata>,
    /// The local-inputs digest in force for this run
    /// (`crate::ids::compute_feed_inputs_digest`), to persist onto
    /// `sources.feed_inputs_digest` **in the same hop** as
    /// [`Self::document_validators`]. `None` for every non-feed source.
    ///
    /// Persisted only alongside the validators, never on its own: the two
    /// are one fact — "these validators were captured under these inputs".
    /// Recording new inputs against validators the run never refreshed would
    /// mark the cache trustworthy for a reprocessing that did not happen,
    /// and the next run would replay them and skip the entry loop, which is
    /// the exact failure the digest exists to prevent.
    pub document_inputs_digest: Option<String>,
}

// ---------------------------------------------------------------------------
// Staleness check
// ---------------------------------------------------------------------------

/// Check if the store's existing data is stale relative to the current policy.
///
/// Returns `true` if the sampled chunk was indexed with a different policy version.
/// Callers should trigger a full reindex when this is true.
///
/// # Note
/// This samples one chunk from the store as a representative. In a consistent
/// store all chunks share the same policy version (reindex is atomic per document),
/// so a single sample is sufficient in practice. If partial-reindex bugs occur,
/// this check may give a false negative; a full scan is not performed for performance.
pub async fn is_store_stale(
    store: &dyn RetrievalStore,
    current_policy_version: &str,
) -> Result<bool, Error> {
    let stats = store.stats().await?;
    if stats.chunk_count == 0 {
        // An empty store is never stale — there is nothing to reindex.
        return Ok(false);
    }

    // Sample one chunk via BM25 to check its policy version.
    //
    // We avoid dense_search here because it requires a query vector whose
    // dimension must match the index.  An empty (&[]) or zero-length vector
    // causes real LanceDB implementations to return an error.
    //
    // The BM25 query uses very common single-character substrings ("e t a")
    // so that any chunk containing typical text will produce a match.  If the
    // store contains only numeric or symbolic content and no result is returned,
    // we conservatively return `false` (not stale) to avoid a spurious reindex.
    let results = store.bm25_search("e t a", 1, &[]).await?;
    if results.is_empty() {
        return Ok(false);
    }

    let sample = &results[0].chunk;
    Ok(sample.policy_version != current_policy_version)
}

// ---------------------------------------------------------------------------
// IndexJob management helpers
// ---------------------------------------------------------------------------

/// Create a new IndexJob in `Pending` state.
pub fn create_index_job(store_id: &str, scope: IndexJobScope) -> IndexJob {
    IndexJob {
        id: new_ulid(),
        store_id: store_id.to_string(),
        scope,
        state: IndexJobState::Pending,
        stats: IndexJobStats::default(),
        error: None,
        error_code: None,
        created_at: now_rfc3339(),
        started_at: None,
        completed_at: None,
    }
}

/// Mark an IndexJob as running.
pub fn start_index_job(job: &mut IndexJob) {
    job.state = IndexJobState::Running;
    job.started_at = Some(now_rfc3339());
}

/// Mark an IndexJob as done with final stats.
pub fn complete_index_job(job: &mut IndexJob, stats: IndexJobStats) {
    job.state = IndexJobState::Done;
    job.stats = stats;
    job.completed_at = Some(now_rfc3339());
}

/// Mark an IndexJob as failed with an unclassified error message — a
/// synthetic queue-level failure (the queue itself is full/closed, or the
/// job's task panicked) that never had a typed `core::Error` to carry a
/// stable code from. `job.error_code` is left `None`; a caller reconstructing
/// the job's error (`cli::job_attach::finish_job`) falls back to
/// `Error::Internal` for these, same as it always has.
pub fn fail_index_job(job: &mut IndexJob, error: String) {
    job.state = IndexJobState::Failed;
    job.error = Some(error);
    job.error_code = None;
    job.completed_at = Some(now_rfc3339());
}

/// Mark an IndexJob as failed from a typed `core::Error`, carrying both its
/// display message (`job.error`) and its stable `code()` string
/// (`job.error_code`) — the pairing `Error::from_code` can invert. This is
/// what lets a daemon-attached job failure surface with the same exit code
/// an embedded pre-flight failure of the same kind would (issue #187
/// review): without it, every job-level failure collapsed to a bare string,
/// indistinguishable from `Error::Internal` once read back by the CLI.
pub fn fail_index_job_with_error(job: &mut IndexJob, error: &Error) {
    job.state = IndexJobState::Failed;
    // Store the bare message (`raw_message()`), not `error.to_string()`:
    // `cli::job_attach::finish_job` reconstructs the typed error via
    // `Error::from_code(error_code, error)`, which re-adds the `Display`
    // prefix (e.g. "invalid config: "). Storing the already-prefixed string
    // would double it (issue #187 review, finding F4). Variants
    // `raw_message()` can't reconstruct fall back to the full `Display`
    // string, since there's no bare field to store instead.
    job.error = Some(
        error
            .raw_message()
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string()),
    );
    job.error_code = Some(error.code().to_string());
    job.completed_at = Some(now_rfc3339());
}

/// Get the current time as an RFC 3339 string.
///
/// Only the clock is stubbed under `cfg(test)` (a fixed instant keeps
/// timestamp-carrying fixtures deterministic); the formatting logic itself is
/// always compiled and unit-tested via [`format_secs_rfc3339`].
pub fn now_rfc3339() -> String {
    #[cfg(not(test))]
    {
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    }
    #[cfg(test)]
    {
        "2026-06-10T12:00:00Z".to_string()
    }
}

/// Format a Unix timestamp as RFC 3339 (UTC, no sub-second precision).
/// Public so callers that need an RFC 3339 string for an instant *other*
/// than now — e.g. `server`'s terminal-job eviction cutoff (now minus a
/// retention grace) — can produce one that compares correctly against
/// [`now_rfc3339`] output.
///
/// The canonical form contract (specs/02-domain-model.md): always
/// `YYYY-MM-DDTHH:MM:SSZ` — `SecondsFormat::Secs` forbids fractional
/// seconds and the trailing `true` forces a literal `Z` rather than
/// `+00:00`, matching every other stored timestamp in the system so plain
/// string comparison stays correct.
pub fn format_secs_rfc3339(secs: u64) -> String {
    // `i64::try_from`, never `as i64`: a wrapping cast turns a `u64` above
    // `i64::MAX` into a *negative* timestamp, which chrono formats happily as
    // a pre-epoch instant (`u64::MAX` becomes `1969-12-31T23:59:59Z`). That
    // is far worse than the out-of-range fallback below — it is a plausible
    // value that silently sorts before every real row. Fail the conversion
    // instead, and let both out-of-range paths share one fallback.
    let formatted = i64::try_from(secs)
        .ok()
        .and_then(|secs| DateTime::<Utc>::from_timestamp(secs, 0))
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
        // Chrono represents years well past 9999 and renders them with a
        // sign prefix (`+10000-01-01T00:00:00Z`), which is not canonical
        // form — and `+` sorts below every digit, so such a value would
        // order before every real row instead of after them. Both
        // out-of-range paths converge on the fallback below.
        .filter(|s| crate::dates::is_canonical_timestamp(s));

    match formatted {
        Some(s) => s,
        // Reachable only for an input no real Unix timestamp carries: above
        // `i64::MAX`, beyond chrono's representable range, or past year
        // 9999. This function is documented as infallible, so rather than
        // introduce a `Result` no caller has a recovery path for, fall back
        // to the epoch: deterministic, never panics, and canonical-form so
        // it still sorts against every other stored value.
        None => "1970-01-01T00:00:00Z".to_string(),
    }
}

#[cfg(test)]
mod format_secs_rfc3339_tests {
    use super::format_secs_rfc3339;

    #[test]
    fn epoch_zero_is_1970_01_01() {
        assert_eq!(format_secs_rfc3339(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn leap_day_2024_02_29_is_formatted_correctly() {
        assert_eq!(format_secs_rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn year_end_boundary_rolls_over_correctly() {
        assert_eq!(format_secs_rfc3339(1_704_067_199), "2023-12-31T23:59:59Z");
        assert_eq!(format_secs_rfc3339(1_704_067_200), "2024-01-01T00:00:00Z");
    }
}

// ---------------------------------------------------------------------------
// Canonical-form contract tests — additional to (not a
// substitute for) the golden `format_secs_rfc3339_tests` exact-value
// assertions above, which are the strongest evidence the chrono
// implementation is behaviorally identical to the hand-rolled arithmetic it
// replaced. These instead guard the *shape* of the contract:
// specs/02-domain-model.md's canonical timestamp form is `YYYY-MM-DDTHH:MM:SSZ`
// — no fractional seconds, `Z` never `+00:00` — and old (hand-rolled-era)
// and new (chrono-era) stored rows must still sort correctly against each
// other by plain string comparison.
#[cfg(test)]
mod canonical_form_tests {
    use super::format_secs_rfc3339;

    /// `^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$`, checked by hand (no regex
    /// dependency in this crate) rather than as a literal pattern.
    use crate::dates::is_canonical_timestamp as matches_canonical_shape;

    /// `now_rfc3339`'s real-clock branch is stubbed to a fixed literal under
    /// `cfg(test)` and never touches the formatter, so this exercises the
    /// real formatter it delegates to instead — the same function
    /// `now_rfc3339`'s non-test branch calls — to prove a freshly produced
    /// value matches the canonical shape.
    #[test]
    fn format_secs_rfc3339_matches_canonical_shape() {
        assert!(
            matches_canonical_shape(&format_secs_rfc3339(1_783_524_645)),
            "expected canonical YYYY-MM-DDTHH:MM:SSZ shape, got: {}",
            format_secs_rfc3339(1_783_524_645)
        );
    }

    /// Guards specifically against a one-word `SecondsFormat::Secs` ->
    /// `SecondsFormat::AutoSi` typo, which would silently reintroduce
    /// variable-precision output and break every stored-value comparison
    /// that assumes a fixed-width timestamp.
    #[test]
    fn format_secs_rfc3339_has_no_fractional_second_component() {
        assert!(!format_secs_rfc3339(1_783_524_645).contains('.'));
    }

    /// Whatever the input, the output is always canonical form. Chrono
    /// happily represents years past 9999 and renders them with a sign
    /// prefix (`+10000-01-01T00:00:00Z`), which sorts below every real row
    /// because `+` is 0x2B — so those must reach the fallback too, not just
    /// values chrono cannot represent at all.
    #[test]
    fn every_timestamp_formats_to_canonical_form_or_falls_back() {
        for secs in [
            0,
            1_781_092_800,
            253_402_300_800,     // 10000-01-01, representable but not RFC 3339
            1_000_000_000_000,   // ~year 33658
            8_210_298_412_800,   // past chrono's range
            i64::MAX as u64 + 1, // past i64
            u64::MAX,
        ] {
            let formatted = format_secs_rfc3339(secs);
            assert!(
                matches_canonical_shape(&formatted),
                "{secs} produced non-canonical {formatted:?}; a `+`/`-` year prefix \
                 sorts below every digit and would misorder against every stored row"
            );
        }
    }

    /// A `u64` above `i64::MAX` must reach the out-of-range fallback, never
    /// wrap into a negative timestamp. A wrapping `as i64` cast turns
    /// `u64::MAX` into `-1`, which chrono formats as `1969-12-31T23:59:59Z`
    /// — a plausible-looking value that would sort before every real row.
    #[test]
    fn timestamps_above_i64_max_fall_back_instead_of_wrapping_to_pre_epoch() {
        for secs in [u64::MAX, i64::MAX as u64 + 1] {
            let formatted = format_secs_rfc3339(secs);
            assert_eq!(
                formatted, "1970-01-01T00:00:00Z",
                "{secs} must hit the epoch fallback, not wrap"
            );
            assert!(
                !formatted.starts_with("1969"),
                "{secs} wrapped into a pre-epoch instant: {formatted}"
            );
        }
    }

    /// The test that actually matters: a hand-written legacy-form literal
    /// (the exact shape hand-rolled `secs_to_ymd_hms` used to produce) and a
    /// freshly chrono-produced value for the same instant — plus values one
    /// second either side — must still order correctly under plain Rust
    /// string comparison. This is what proves old rows (written before this
    /// migration) and new rows (written after) sort together correctly; a
    /// shape check alone does not.
    #[test]
    fn legacy_literal_and_chrono_produced_value_interleave_correctly() {
        // 2026-06-10T12:00:00Z, the same instant `now_rfc3339`'s cfg(test)
        // branch returns as a literal.
        const AT_SECS: u64 = 1_781_092_800;
        let legacy_literal = "2026-06-10T12:00:00Z".to_string();
        let chrono_produced = format_secs_rfc3339(AT_SECS);
        let one_second_before = format_secs_rfc3339(AT_SECS - 1);
        let one_second_after = format_secs_rfc3339(AT_SECS + 1);

        assert_eq!(legacy_literal, chrono_produced);
        assert!(one_second_before < legacy_literal);
        assert!(one_second_before < chrono_produced);
        assert!(legacy_literal < one_second_after);
        assert!(chrono_produced < one_second_after);
    }
}

/// Why the delete-sweep (path/url) or the feed liveness sweep (feed) was
/// suppressed this run — guard 1 or guard 2, documented at the
/// `suppressed_because` computation in `run_source_ingestion` below.
///
/// Both guards suppress the presumed-gone sweep for path/url sources, and
/// both are logged at `warn!` there: either one means a run that should have
/// produced full evidence didn't, which is anomalous regardless of which
/// guard caught it.
///
/// The feed liveness sweep inherits only `IncompleteEnumeration`. The
/// zero-seen backstop is the routine steady state for a feed — the feed
/// document's own 304 short-circuit fires zero entry callbacks — and, more
/// importantly, absence there only decides who gets *probed*; the delete
/// still needs a confirmed 404/410. See the feed branch in
/// `run_source_ingestion` and specs/04-search-pipeline.md §1 "Guards".
enum SweepSuppression {
    /// Guard 1 — the ingestor itself reported it could not observe the
    /// source. Always anomalous.
    IncompleteEnumeration(String),
    /// Guard 2 — a run that claimed complete enumeration nonetheless
    /// observed none of the source's previously owned URIs.
    ZeroSeen,
}

impl SweepSuppression {
    /// The reason clause both suppression warnings interpolate.
    fn reason(&self) -> &str {
        match self {
            SweepSuppression::IncompleteEnumeration(reason) => reason,
            SweepSuppression::ZeroSeen => {
                "this run observed none of the documents this source owns"
            }
        }
    }
}

/// Remove one URI's indexed content, or — under [`DeletionPolicy::Retain`] —
/// merely count it as prunable.
///
/// Shared by the two removal paths in [`run_source_ingestion`], which differ
/// only in *which* URIs they hand here: the confirmed-gone path passes URIs
/// the origin answered 404/410 for, the presumed-gone sweep passes URIs this
/// run never observed. What happens to a URI once one of them has decided is
/// identical, and stating it twice invites the two to drift.
///
/// The liveness sweep deliberately does not route through this: it already
/// holds the candidate's `resource_id` from its own store query, so it never
/// needs `doc_index.remove`'s return value, and it runs only under
/// [`DeletionPolicy::Prune`] so it has no retaining branch to share.
async fn prune_or_count(
    uri: &str,
    deletion: DeletionPolicy,
    doc_index: &mut DocumentIndex,
    store: &dyn RetrievalStore,
    result: &mut IngestionResult,
) -> Result<(), Error> {
    if deletion == DeletionPolicy::Retain {
        result.docs_prunable += 1;
        return Ok(());
    }
    if let Some(old_record) = doc_index.remove(uri) {
        let deleted = store.delete_by_resource(&old_record.resource_id).await?;
        if deleted > 0 {
            result.docs_deleted += 1;
        }
    }
    Ok(())
}

/// Run the unified ingestion pipeline for one source, driven by a caller-supplied
/// `&dyn Ingestor` (issue #117; specs/01-architecture.md §1).
///
/// Streams `Resource`s one at a time via [`PipelineCallback`] — no buffering of
/// an entire source's resources in memory. Per resource: skip-check (unchanged
/// `content_hash` + `policy_version`) → [`index_resource`] → counters/progress.
/// Per-resource errors become stats counters and progress events, never abort
/// the run.
///
/// # Removal
///
/// Nothing is ever removed unless `deps.deletion` is [`DeletionPolicy::Prune`]
/// — deletion is opt-in, like `rsync --delete`. A retaining run counts what it
/// would have removed into [`IngestionResult::docs_prunable`] instead.
///
/// Under `Prune`, two separate paths remove documents, and the difference
/// between them is the subject of issues #156/#185:
///
/// - **Confirmed gone.** A URI reported via `IngestCallback::on_gone` (an HTTP
///   404/410 after retry) is deleted unconditionally. The origin was reached
///   and answered — that is knowledge, so no guard applies.
/// - **Presumed gone (the delete-sweep).** A URI previously indexed for this
///   source that was neither yielded nor reported via `on_skipped` is
///   *inferred* to be gone. Because that is an inference from absence, it is
///   swept only when the absence is informative: feed sources are exempt
///   entirely (a feed is a bounded window), an incomplete enumeration
///   suppresses it (guard 1), and so does a run that observed none of the
///   source's own URIs (guard 2). See the comments at the sweep below.
pub async fn run_source_ingestion(
    source: &Source,
    ingestor: &dyn Ingestor,
    deps: SourceIngestionDeps<'_>,
) -> Result<IngestionResult, Error> {
    let SourceIngestionDeps {
        doc_index,
        store,
        embedder,
        config,
        progress,
        deletion,
        document_validators,
        stored_inputs_digest,
        fetcher,
    } = deps;

    // The origin's validator speaks for the origin's bytes. It cannot speak
    // for our indexing policy, whether we follow entry links, or how many
    // entries we take — so a change to any of those must not be allowed to
    // hide behind a 304 that skips the entry loop entirely. Compared here,
    // in `core`, rather than inside the feed ingestor: this mirrors
    // `PipelineCallback::lookup_fetch_metadata`'s `policy_version`
    // suppression one layer down, and keeps both halves of the same rule in
    // the same crate. See `specs/02-domain-model.md`'s Feed connector,
    // "Conditional GET and pruning".
    let document_inputs_digest = feed_inputs_digest(source, config);
    let document_validators = match &document_inputs_digest {
        Some(current) if stored_inputs_digest.as_deref() != Some(current.as_str()) => {
            tracing::debug!(
                source_id = %source.id,
                "feed inputs changed since the stored validators were captured; \
                 fetching the feed document unconditionally"
            );
            FetchMetadata::default()
        }
        _ => document_validators,
    };

    let ingest_config = serde_json::to_value(&source.spec).map_err(|e| Error::Internal {
        message: format!("failed to serialize source spec: {e}"),
        correlation_id: "source_spec_serialize".to_string(),
    })?;

    let ingest_source = IngestSource {
        source_id: source.id.clone(),
        store_id: source.store_id.clone(),
        ingestor_kind: ingestor.kind(),
        config: ingest_config,
        policy_version: config.policy_version.clone(),
        document_validators,
    };

    if let Some(sink) = &progress {
        sink(crate::progress::ProgressEvent::SourceStarted {
            source_id: source.id.clone(),
            location: source_location(source),
        });
    }

    let mut callback = PipelineCallback {
        source,
        doc_index,
        store,
        embedder,
        config,
        progress: progress.clone(),
        result: IngestionResult::default(),
        seen: std::collections::HashSet::new(),
        gone: std::collections::HashSet::new(),
        discovered_total: 0,
        next_index: 0,
        skip_error_count: 0,
    };

    let ingest_result = ingestor.ingest(&ingest_source, &mut callback).await?;

    let PipelineCallback {
        mut result,
        seen,
        gone,
        doc_index,
        skip_error_count,
        ..
    } = callback;

    // C8: `result.error_count` (below) is already authoritative — every
    // error path an ingestor takes must report `on_skipped(SkipReason::Error)`
    // exactly once (which increments `skip_error_count` here and
    // `result.error_count` above), and `PipelineCallback::on_resource`'s
    // `Err(e)` arm additionally counts `index_resource` failures the
    // ingestor never sees. So `ingest_result.errors` (the ingestor's own,
    // narrower self-report) is intentionally NOT folded into
    // `result.error_count` here — doing so would double-count every error
    // the ingestor already surfaced via `on_skipped`. It's used only as a
    // consistency check: a well-behaved ingestor's own error counter must
    // exactly match the number of `SkipReason::Error` skips it reported this
    // run. A mismatch means an ingestor bumped `IngestResult.errors` without
    // (or instead of) calling `on_skipped(Error)`, silently keeping a dead
    // URI alive in the sweep (or vice versa) — a bug in that ingestor, not
    // in the pipeline.
    debug_assert_eq!(
        ingest_result.errors, skip_error_count,
        "ingestor for source {} reported {} internal errors but only {} were \
         surfaced via on_skipped(SkipReason::Error) — every error path must \
         report exactly one SkipReason::Error skip",
        source.id, ingest_result.errors, skip_error_count
    );

    result.document_validators = ingest_result.document_validators.clone();
    result.document_inputs_digest = document_inputs_digest;

    // Confirmed deletions first, and unconditionally: a URI reported via
    // `on_gone` was positively established as absent at the origin (an HTTP
    // 404/410 after retry). That is *knowledge*, not inference — the origin
    // was reached and answered — so none of the sweep's guards below apply to
    // it, and neither does the feed exemption: a feed entry whose linked page
    // is confirmed 410 is gone whether or not the feed window still lists it.
    //
    // This is the distinction the rest of this function is organized around.
    // Knowing a resource is gone is not the same as failing to find it; only
    // the latter needs guarding, because only the latter is inferred.
    //
    // Both still answer to `deletion`, though: "the origin no longer has this"
    // is not the same as "you no longer want this." A local copy of a page
    // that has since 404'd is often the most valuable thing in the index.
    for uri in &gone {
        let owned_by_this_source = doc_index
            .get(uri)
            .is_some_and(|record| record.source_id == source.id);
        if !owned_by_this_source {
            continue;
        }
        prune_or_count(uri, deletion, doc_index, store, &mut result).await?;
    }

    // Delete-sweep: any URI known to this source's doc_index that was neither
    // yielded (on_resource) nor reported skipped (on_skipped) this run is
    // *presumed* gone — delete it. A deleted file simply isn't enumerated
    // again. Unlike the `on_gone` path above, this is an inference from
    // absence, which is why it is guarded.
    //
    // Ownership is decided by `source_id`, never by comparing URI strings
    // against the source's configured root/URL. The doc_index is store-wide,
    // and URI-shape heuristics misattribute rows across sources: a root that
    // is a string prefix of a sibling's (`/data/blog` vs `/data/blog-drafts`),
    // or percent-encoding twins (a literal `foo%23` directory vs a `foo#`
    // directory, whose canonical URIs are byte-identical), would let sweeping
    // one source delete another source's live documents. `source_id` is exact:
    // it is persisted per resource (baseline schema), rehydrated by
    // `list_indexed_documents`, and immune to encoding.
    // C1: feed sources are exempt from the delete-sweep. A feed only ever
    // exposes its most-recent N entries (an Atom/RSS document is a bounded
    // window, not a full archive listing) — an entry's absence from this
    // run means only "it scrolled off the feed," not "it was deleted at the
    // origin." Sweeping on that basis would delete everything the feed
    // previously contributed as soon as it aged out of the window, and a
    // feed-level 304 Not Modified (zero callbacks at all) would make the
    // sweep delete the *entire* source on every unchanged poll. Path and
    // url sources have no such windowing — their ingestor enumerates the
    // full current state every run — so absence there really does mean
    // deletion and the sweep must still run for them. This exemption is
    // unchanged by the feed liveness sweep below: that sweep only ever
    // deletes on a positively confirmed 404/410, never on absence alone, so
    // it belongs to a different bucket entirely (the same one the `on_gone`
    // loop above does) and doesn't need this guard reasoning to justify it.
    //
    // Ownership + guard evaluation below is shared by this sweep (path/url
    // sources) and the feed liveness sweep further down (feed sources):
    // both infer something from "this run observed none of the URIs this
    // source owns," so both need the same two guards computed from the same
    // owned/seen sets. Computed unconditionally rather than only inside the
    // `!Feed` branch — feed sources need it just as much; see the liveness
    // sweep's own guard handling below, in particular how it reconciles
    // guard 2 with the feed document's own 304 short-circuit.
    let owned_uris: Vec<String> = doc_index
        .uris()
        .into_iter()
        .filter(|uri| {
            doc_index
                .get(uri)
                .is_some_and(|record| record.source_id == source.id)
        })
        .collect();
    let any_owned_uri_seen = owned_uris.iter().any(|uri| seen.contains(uri));

    // Two further suppressions, both from issue #156, both stating the rule
    // the feed exemption above states for feeds: the sweep infers deletion
    // (or, for the liveness sweep, "eligible to probe") from absence, so it
    // may only run when the absence is *informative*.
    let suppressed_because = match &ingest_result.enumeration {
        // Guard 1 — enumeration completeness. The ingestor itself reported
        // that it could not observe the source (an unmounted volume, an
        // unreachable root, an API that failed part-way). Its silence
        // about a URI says nothing about whether that URI still exists.
        // This is the guard that fires for the reported incident:
        // `/Volumes/Archive` unmounted, `FileIngestor` enumerated zero
        // files, and the sweep deleted every document the source owned.
        Enumeration::Incomplete { reason } => {
            Some(SweepSuppression::IncompleteEnumeration(reason.clone()))
        }
        // Guard 2 — zero-seen backstop, source-shape-agnostic. Even with a
        // *complete* enumeration claimed, a source that previously owned
        // documents and observed none of them this run is far more likely
        // to be a broken connector than a source whose entire contents
        // vanished at once. This does not subsume guard 1: a connector
        // that enumerates 3 of 500 items before failing has a non-empty
        // `seen` set, so only guard 1 protects the other 497. For a feed
        // source specifically, this is also what reconciles the liveness
        // sweep with the feed document's own 304 short-circuit: a 304 fires
        // zero entry callbacks, so `seen` is empty and this guard fires —
        // correctly, since an unchanged feed document means an unchanged
        // window and nothing can have aged out since the last run.
        //
        // Deliberate trade-off: a source whose files really were all
        // deleted or renamed in one run keeps its stale documents until
        // the source is re-created. The warning below says so.
        Enumeration::Complete if !owned_uris.is_empty() && !any_owned_uri_seen => {
            Some(SweepSuppression::ZeroSeen)
        }
        Enumeration::Complete => None,
    };

    if !matches!(source.spec, SourceSpec::Feed { .. }) {
        if let Some(reason) = &suppressed_because {
            tracing::warn!(
                source_id = %source.id,
                location = %source_location(source),
                documents_preserved = owned_uris.len(),
                "skipping delete-sweep for source at '{}': {}. {} previously \
                 indexed document(s) were left in place rather than deleted, \
                 because this run produced no evidence that they are gone. If \
                 the source really is empty now, remove and re-add it \
                 (`localdb source remove` / `localdb source add`) and reindex.",
                source_location(source),
                reason.reason(),
                owned_uris.len(),
            );
        } else {
            for uri in owned_uris {
                if seen.contains(&uri) || gone.contains(&uri) {
                    continue;
                }
                prune_or_count(&uri, deletion, doc_index, store, &mut result).await?;
            }
        }
    } else if deletion == DeletionPolicy::Prune {
        // The feed liveness sweep (specs/04-search-pipeline.md §1 "Aged-out
        // feed entries: the liveness sweep"). Gated on `Prune` here, not
        // inside `run_feed_liveness_sweep` itself: a retaining run performs
        // zero liveness fetches and reports nothing pruned or prunable for
        // this mechanism — there is no free preview signal for it the way
        // `docs_prunable` is one for the presumed-gone sweep, since the only
        // way to learn anything here is a network request per candidate.
        // The liveness sweep inherits *one* of the two guards, not both
        // (specs/04-search-pipeline.md §1 "Guards"). The two sweeps read the
        // same seen-set for different purposes: the presumed-gone sweep
        // deletes on absence, so an untrustworthy seen-set is an
        // untrustworthy delete signal, while here absence only decides who
        // gets probed and the delete needs a confirmed 404/410 from the
        // origin.
        if let Some(reason @ SweepSuppression::IncompleteEnumeration(_)) = &suppressed_because {
            // Guard 1 stays anomalous for a feed exactly as it is for
            // path/url sources: the ingestor itself failed to observe the
            // source, so it knows nothing about which entries the window
            // holds. Every previously indexed URI would look aged out and the
            // source's whole document set would queue for probing, 25 per
            // run, off a signal already known to be broken.
            tracing::warn!(
                source_id = %source.id,
                location = %source_location(source),
                "skipping feed liveness sweep for source at '{}': {}. This is \
                 the same signal that suppresses the presumed-gone sweep for \
                 path/url sources above: a run that could not read the feed's \
                 window cannot tell an aged-out entry from one it simply never \
                 saw.",
                source_location(source),
                reason.reason(),
            );
        } else {
            // Guard 2 — the zero-seen backstop — deliberately does NOT
            // suppress this sweep, even though it is the routine steady state
            // for a feed under `--delete`: the feed document's own 304 fires
            // zero entry callbacks, so `seen` is empty on every quiet run.
            // Suppressing here starved the mechanism in precisely the case it
            // exists for — a feed goes quiet, every subsequent run 304s, and
            // the aged-out backlog is never probed again, this sweep being the
            // only thing that could ever shrink it.
            //
            // Running is safe because both bounds that make the sweep safe at
            // all are independent of the seen-set: it deletes only on a
            // confirmed 404/410, and it probes at most 25 candidates per run
            // per source, none more often than the recheck floor allows. An
            // empty seen-set subtracts nothing from the candidate list, which
            // is the right answer for a 304'd run — the window is unchanged,
            // so nothing aged out *during* this run, and every candidate the
            // query returns had already aged out before it began.
            let refresh_interval_secs = match &source.spec {
                SourceSpec::Feed {
                    refresh_interval_secs,
                    ..
                } => *refresh_interval_secs,
                _ => None,
            };
            run_feed_liveness_sweep(
                &source.id,
                &source.store_id,
                refresh_interval_secs,
                &seen,
                doc_index,
                store,
                fetcher,
                &mut result,
            )
            .await?;
        }
    }

    if let Some(sink) = &progress {
        sink(crate::progress::ProgressEvent::SourceFinished {
            result: result.clone(),
        });
    }

    Ok(result)
}

/// Human-readable "location" string for `ProgressEvent::SourceStarted`.
fn source_location(source: &Source) -> String {
    match &source.spec {
        SourceSpec::Path { root, .. } => root.clone(),
        SourceSpec::Url { url, .. } => url.clone(),
        SourceSpec::Feed { url, .. } => url.clone(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
