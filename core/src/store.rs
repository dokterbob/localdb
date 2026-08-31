//! The `RetrievalStore` trait and related types.
//!
//! This is the abstraction layer between `core` domain logic and the physical
//! storage backend. The default implementation is in `store-libsql`.
//!
//! Fusion (RRF) happens **above** this trait in `core` — the trait exposes raw
//! BM25 and dense search legs separately.
//!
//! See specs/01-architecture.md §4 and specs/04-search-pipeline.md §5.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
#[cfg(any(test, feature = "test-support"))]
use std::collections::HashMap;

use crate::ids::{ContentId, UlidId};
use crate::ingestion::DocumentRecord;
use crate::metadata::Metadata;
use crate::types::{Chunk, Span};
use crate::Error;

// ---------------------------------------------------------------------------
// ChunkRecord — the unit stored in a backend
// ---------------------------------------------------------------------------

/// A chunk record as stored in the retrieval backend.
///
/// This contains all fields needed for BM25, dense search, and citation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChunkRecord {
    /// Content-addressed chunk ID.
    pub id: ContentId,

    /// Parent document ID.
    pub resource_id: ContentId,

    /// Owning store ID.
    pub store_id: UlidId,

    /// Chunk text (feeds BM25).
    pub text: String,

    /// Range in the normalized document text.
    pub span: Span,

    /// Heading path inherited from blocks.
    #[serde(default)]
    pub heading_path: Vec<String>,

    /// Dense embedding vector.
    pub embedding: Vec<f32>,

    /// Hash of the indexing policy that produced this chunk.
    pub policy_version: String,

    /// Acquisition time (RFC 3339 string). Used for metadata filters.
    pub fetched_at: String,

    /// The resource's claimed content modification time (RFC 3339 string) —
    /// the origin's own notion of "last changed" (e.g. a feed entry's
    /// `<updated>`), distinct from `fetched_at` (when *our store* acquired
    /// it). `None` when the source makes no such claim. Written to the
    /// nullable `resources.modified_at` column. See specs/02-domain-model.md
    /// §2.
    pub modified_at: Option<String>,

    /// blake3 content hash of normalized text (hex string).
    pub content_hash: String,

    /// Origin store ID (for federation provenance).
    pub origin_store: UlidId,

    /// Source ID.
    pub source_id: UlidId,

    /// Source kind (e.g. "path", "url").
    pub ingestor_kind: String,

    /// MIME type for metadata filtering.
    #[serde(default)]
    pub mime: Option<String>,

    /// Document URI (e.g. `file:///path/to/file` or URL).
    pub uri: String,

    /// Resource metadata, tagged by resource kind.
    ///
    /// Persisted as a JSON-encoded column (`"kind":"document"|"conversation"|"transcription"`
    /// plus the flattened Dublin Core fields). Read defensively: rows written
    /// before this schema migration (untagged, flat Dublin Core JSON) fall
    /// back to `Metadata::default()` on read rather than erroring.
    #[serde(default)]
    pub metadata: Metadata,

    /// Block sequence number (populated from ChunkOutput.block_seq).
    #[serde(default)]
    pub block_seq: u32,

    /// Chunk position within the block (populated from ChunkOutput.seq_in_block).
    #[serde(default)]
    pub seq_in_block: u32,

    /// Block kind string (e.g. "text", "heading").
    ///
    /// `None` for chunks indexed before the Resource/Block architecture
    /// was introduced.
    #[serde(default)]
    pub block_kind: Option<String>,

    /// 1-indexed page number of the originating block, for paginated source
    /// formats (#103). Copied from the block's `location.page`; `None` for
    /// non-paginated formats and rows written before page plumbing existed.
    /// Persisted inside `location_json` as an optional `"page"` key.
    #[serde(default)]
    pub page: Option<u32>,

    /// For message-window chunks (#129): all block seqs participating in the
    /// window. Empty for ordinary single-block chunks. Persisted inside
    /// `location_json` as `{"start", "end", "window_block_seqs"?}`, present
    /// only when non-empty.
    #[serde(default)]
    pub window_block_seqs: Vec<u32>,

    /// The resource's own claimed date, exactly as the source expressed it
    /// (a PDF `D:` string's date portion, an EPUB OPF `dc:date`, a feed
    /// entry's `published`/`updated`). Write-only: persisted to
    /// `resources.date_original` on every upsert but never read back onto a
    /// `ChunkRecord` (not part of `CHUNK_COLS`) — nothing currently consumes
    /// it from a chunk read. See specs/02-domain-model.md §2.
    #[serde(default)]
    pub date_original: Option<String>,

    /// `date_original` normalized to a sortable ISO 8601 string via
    /// `crate::dates::parse_partial_iso8601`, or `None` when `date_original`
    /// was absent or unparseable. Write-only, same posture as
    /// `date_original`.
    #[serde(default)]
    pub date_parsed: Option<String>,

    /// The source's own identifier for this resource (e.g. a feed entry's
    /// `<id>`), distinct from localdb's content-addressed `resource_id`.
    /// Write-only, same posture as `date_original`.
    #[serde(default)]
    pub external_id: Option<String>,

    /// The source's own change-detection token for this resource (e.g. an
    /// HTTP `ETag`). Write-only, same posture as `date_original`.
    #[serde(default)]
    pub external_etag: Option<String>,
}

impl ChunkRecord {
    /// Construct a `ChunkRecord` from a `Chunk` plus supplementary fields.
    pub fn from_chunk(
        chunk: &Chunk,
        embedding: Vec<f32>,
        uri: String,
        mime: Option<String>,
        metadata: Metadata,
    ) -> Self {
        Self {
            id: chunk.id.clone(),
            resource_id: chunk.resource_id.clone(),
            store_id: chunk.store_id.clone(),
            text: chunk.text.clone(),
            span: chunk.span.clone(),
            heading_path: chunk.heading_path.clone(),
            embedding,
            policy_version: chunk.policy_version.clone(),
            fetched_at: chunk.provenance.fetched_at.clone(),
            // `Chunk`/`Provenance` carry no modified_at of their own (see
            // `Provenance`'s doc comment — acquisition time only); default to
            // `None` (no claim) here for callers that never touch this field.
            // `index_resource` overrides this with the real
            // `resource.modified_at` right after constructing the record.
            modified_at: None,
            content_hash: chunk.provenance.content_hash.clone(),
            origin_store: chunk.provenance.origin_store.clone(),
            source_id: chunk.provenance.source_ref.id.clone(),
            ingestor_kind: chunk.provenance.source_ref.kind.clone(),
            mime,
            uri,
            metadata,
            block_seq: 0,
            seq_in_block: 0,
            block_kind: None,
            page: None,
            window_block_seqs: chunk.window_block_seqs.clone(),
            // Not derivable from `Chunk`/`Provenance` — `index_resource`
            // stamps these onto each record after construction, same as
            // `modified_at` above.
            date_original: None,
            date_parsed: None,
            external_id: None,
            external_etag: None,
        }
    }
}

// ---------------------------------------------------------------------------
// ResourceRecord — metadata-only update payload
// ---------------------------------------------------------------------------

/// The fields a metadata-only update (issue #176) writes to an existing
/// resource row, without touching its chunks, blocks, or embeddings.
///
/// `store_id`/`resource_id` are passed as `update_resource_metadata`
/// parameters rather than struct fields, matching the trait's other
/// per-call methods (`delete_by_resource`, `get_chunks_for_resource`, ...).
/// `title` is deliberately omitted: it is always derived from
/// `metadata.title()` (Dublin Core), the same convention
/// `upsert_chunks_inner` follows for the full-write path — a separate
/// `title` field here would let the two disagree. `index_updated_at` is
/// likewise omitted: the store stamps it itself with its own write-time
/// clock reading, mirroring `upsert_chunks_inner`'s single
/// `now_rfc3339()` call for a batch.
///
/// Named ahead of the broader per-resource CRUD surface issue #189 previews
/// (extractor_version, etc.) — kept to plain fields deliberately so a future
/// field there is a small addition, not a redesign.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceRecord {
    /// Resource metadata, tagged by resource kind — already post-backfill
    /// (see `core::ids::compute_metadata_hash`'s doc comment): the caller
    /// must have already folded `resource.title` into
    /// `metadata.dublin_core_mut().title` when the metadata itself carried
    /// none, exactly as `index_resource` does for the full-write path.
    pub metadata: Metadata,
    /// The source's own identifier for this resource. See `ChunkRecord::external_id`.
    pub external_id: Option<String>,
    /// The source's own change-detection token. See `ChunkRecord::external_etag`.
    pub external_etag: Option<String>,
    /// Raw HTTP `Last-Modified` conditional-GET validator, beside
    /// `external_etag`. See `Resource::external_last_modified` — not an
    /// input to `core::ids::compute_metadata_hash`.
    pub external_last_modified: Option<String>,
    /// The resource's own claimed modification time (RFC 3339). `None` when
    /// the source makes no such claim — see `ChunkRecord::modified_at`.
    pub modified_at: Option<String>,
    /// The resource's own claimed date, exactly as the source expressed it.
    /// See `ChunkRecord::date_original`.
    pub date_original: Option<String>,
    /// `date_original` normalized via `crate::dates::parse_partial_iso8601`.
    /// See `ChunkRecord::date_parsed`.
    pub date_parsed: Option<String>,
}

// ---------------------------------------------------------------------------
// StaleFeedResource — feed liveness sweep candidate
// ---------------------------------------------------------------------------

/// A feed-discovered resource eligible for a liveness probe: this run did
/// not observe it, so — from the store's point of view alone — it may have
/// aged out of the feed's window. See
/// `RetrievalStore::list_stale_feed_resources` and
/// specs/04-search-pipeline.md §1 "Aged-out feed entries: the liveness
/// sweep".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleFeedResource {
    /// The resource's id (`resources.id`) — the key `delete_by_resource` and
    /// `touch_resource_liveness` both take.
    pub resource_id: String,
    /// The resource's own URI — the entry link the sweep probes.
    pub uri: String,
    /// Stored `ETag` validator, replayed as `If-None-Match`.
    pub external_etag: Option<String>,
    /// Stored `Last-Modified` validator, replayed as `If-Modified-Since`.
    pub external_last_modified: Option<String>,
}

// ---------------------------------------------------------------------------
// SearchResult
// ---------------------------------------------------------------------------

/// A single search result from one leg (dense or BM25).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    /// The matching chunk record.
    pub chunk: ChunkRecord,

    /// The score for this result within its leg.
    /// Dense: cosine/dot-product similarity.
    /// BM25: BM25 score.
    ///
    /// # Cross-store comparability
    ///
    /// Multi-store search pools every queried store's results for a leg into
    /// one ranking ordered by this raw score, before a single RRF pass (see
    /// `search::pool_leg_results`). That is only meaningful if every store
    /// queried together reports that leg's scores on the **same** scale *and*
    /// with the same distribution. Two ways that can break:
    ///
    /// - **Unbounded vs bounded.** This doc permits "cosine/dot-product", but
    ///   an unbounded dot-product would outrank a bounded cosine similarity
    ///   regardless of true relevance. Note the default embedding model emits
    ///   *unnormalized* vectors and documents cosine as required, so a
    ///   dot-product dense score would be wrong for it independently of
    ///   pooling. Dense scores must be a bounded similarity in `[0, 1]`.
    /// - **Same range, different distribution.** `store-libsql` already maps
    ///   distance to score two ways, chosen per store by the encoding its
    ///   embedder produced ([`crate::embedder::Embedder::vector_encoding`]):
    ///   `1 - d/2` from a continuous cosine distance for `Float32`, and
    ///   `1 - d/nbits` from a sign-only binarized Hamming distance for
    ///   `Binary` — which is what the default Perplexity local model emits.
    ///   Both land in `[0, 1]`, but they are not the same distribution, so
    ///   pooling them together would favor whichever runs hotter rather than
    ///   whichever is more relevant. The two shipped models differ in
    ///   dimensionality (1024 vs 384), so a single query cannot currently hit
    ///   both — but nothing enforces that, and it is not a property to rely on.
    ///
    /// BM25 scores are inherently corpus-relative (per-store IDF and average
    /// document length), so they are only approximately comparable across
    /// stores even when every store runs the same backend. Calibrating both
    /// legs is tracked by #40.
    pub score: f32,
}

// ---------------------------------------------------------------------------
// DateAxis — which of the four date signals a date filter bounds
// ---------------------------------------------------------------------------

/// Which of the four date axes (specs/02-domain-model.md §"Date axes
/// (normative)") a [`MetadataFilter::DateAfter`]/[`MetadataFilter::DateBefore`]
/// bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DateAxis {
    /// `resources.added_at` — when we first indexed this resource version.
    Added,
    /// `resources.index_updated_at` — when we last wrote its stored state.
    Updated,
    /// `resources.modified_at` — the source's own claim about last change.
    Modified,
    /// `resources.date_parsed` — the document's own Dublin Core `dc:date`.
    Document,
}

impl DateAxis {
    /// All four axes, in the order §"Date axes (normative)" lists them.
    pub const ALL: [DateAxis; 4] = [
        DateAxis::Added,
        DateAxis::Updated,
        DateAxis::Modified,
        DateAxis::Document,
    ];

    /// The public name shared by every surface (CLI flags, MCP tool params,
    /// HTTP query params). Deliberately "document", not "date_parsed" or
    /// "date" — it must never leak the storage column name.
    pub fn name(&self) -> &'static str {
        match self {
            DateAxis::Added => "added",
            DateAxis::Updated => "updated",
            DateAxis::Modified => "modified",
            DateAxis::Document => "document",
        }
    }

    /// The `resources` column this axis reads/filters on.
    pub fn column(&self) -> &'static str {
        match self {
            DateAxis::Added => "added_at",
            DateAxis::Updated => "index_updated_at",
            DateAxis::Modified => "modified_at",
            DateAxis::Document => "date_parsed",
        }
    }

    /// One-line human description of this axis, shared verbatim by every
    /// surface's `--help`/schema text. `#[arg(long = ...)]` and
    /// `#[schemars(description = ...)]` are derive-macro attributes parsed as
    /// literal tokens at macro-expansion time — a runtime `&'static str`
    /// cannot appear inside them — so the CLI flag help (`localdb/src/main.rs`)
    /// and the MCP tool schema (`SearchFilters` field docs) each hand-write
    /// their own copy of this text rather than calling this function
    /// directly. This function exists so a single consistency test can
    /// assert those hand-written copies actually contain it, rather than
    /// trusting 24 independently-edited literals to stay in sync.
    pub fn describe(&self) -> &'static str {
        match self {
            DateAxis::Added => "when this resource was first indexed",
            DateAxis::Updated => "when the store last wrote this resource's stored state",
            DateAxis::Modified => "the source's own claim of when this resource was last changed",
            DateAxis::Document => "the document's own claimed date (Dublin Core dc:date)",
        }
    }

    /// The Rust-side value for this axis on a `ChunkRecord`. Backs the
    /// in-process (`FakeStore`/test-support) matching path in
    /// [`MetadataFilter::matches`] only — the real (libsql) backend filters
    /// entirely in SQL via [`DateAxis::column`] and never calls this.
    ///
    /// `DateAxis::Updated` always returns `None`: `index_updated_at` is
    /// stamped by the store itself at write time — both
    /// `upsert_chunks_inner` and `update_resource_metadata_inner` in
    /// `store-libsql/src/tenant/write.rs` compute it fresh from
    /// `now_rfc3339()` and never read a record-supplied value — so no
    /// `ChunkRecord` field could ever agree with what actually lands in the
    /// column. Returning `None` here (which `matches` treats as "fails every
    /// bound") is the honest answer: a populated field would silently
    /// diverge between `FakeStore` (authoritative) and the real backend
    /// (write-ignored).
    fn value_of<'a>(&self, record: &'a ChunkRecord) -> Option<&'a str> {
        match self {
            DateAxis::Added => Some(record.fetched_at.as_str()),
            DateAxis::Updated => None,
            DateAxis::Modified => record.modified_at.as_deref(),
            DateAxis::Document => record.date_parsed.as_deref(),
        }
    }
}

// ---------------------------------------------------------------------------
// MetadataFilter — pushed down to the backend
// ---------------------------------------------------------------------------

/// A single metadata filter condition.
///
/// See specs/04-search-pipeline.md §5 (filter pushdown expectations).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MetadataFilter {
    /// Filter by MIME type.
    Mime(String),
    /// Filter by URI prefix.
    UriPrefix(String),
    /// Inclusive lower bound on `axis`.
    DateAfter { axis: DateAxis, value: String },
    /// Inclusive upper bound on `axis`.
    DateBefore { axis: DateAxis, value: String },
    /// Filter by source ID.
    SourceId(UlidId),
    /// Filter by document ID.
    ResourceId(ContentId),
    /// Filter by policy version.
    PolicyVersion(String),
}

impl MetadataFilter {
    pub fn matches(&self, record: &ChunkRecord) -> bool {
        match self {
            MetadataFilter::Mime(mime) => record.mime.as_deref() == Some(mime.as_str()),
            MetadataFilter::UriPrefix(prefix) => record.uri.starts_with(prefix.as_str()),
            // NULL fails every bound (no `Some(v)` to compare), matching
            // SQL's `NULL >= 'x'` falsy behavior for the nullable
            // `modified_at`/`date_parsed` axes — see this type's doc comment
            // and specs/02-domain-model.md §"Date axes (normative)".
            MetadataFilter::DateAfter { axis, value } => {
                axis.value_of(record).is_some_and(|v| v >= value.as_str())
            }
            MetadataFilter::DateBefore { axis, value } => axis.value_of(record).is_some_and(|v| {
                // The two operands widen under DIFFERENT rules, because they
                // become partial-width for different reasons:
                //
                // - The BOUND is whatever a caller supplied, so it can be
                //   partial on ANY axis. It is always widened. Without this,
                //   an inclusive `added_before: "2026"` excludes every
                //   resource added during 2026: a longer string sorts after
                //   its own prefix, so `"2026-06-10T12:00:00Z" <= "2026"` is
                //   false.
                // - The STORED value is only partial-width on `Document`
                //   (`date_parsed` is normalized to exactly 4, 7, or 10 chars
                //   by `crate::dates::parse_partial_iso8601`);
                //   `Added`/`Updated`/`Modified` are always full RFC 3339.
                //   Widening those would be a no-op, so it is skipped —
                //   matching the SQL side, which pays for a `CASE` over the
                //   column only on `Document`.
                //
                // This arm MUST stay in lockstep with `build_filter_clauses`
                // in `store-libsql/src/tenant/sql.rs`, which mirrors exactly
                // this split.
                let bound = crate::dates::widen_date_upper_bound(value);
                match axis {
                    DateAxis::Document => crate::dates::widen_date_upper_bound(v) <= bound,
                    _ => v <= bound.as_str(),
                }
            }),
            MetadataFilter::SourceId(id) => &record.source_id == id,
            MetadataFilter::ResourceId(id) => &record.resource_id == id,
            MetadataFilter::PolicyVersion(v) => &record.policy_version == v,
        }
    }
}

// ---------------------------------------------------------------------------
// StoreStats
// ---------------------------------------------------------------------------

/// Statistics for a retrieval store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StoreStats {
    /// Number of chunks indexed.
    pub chunk_count: u64,
    /// Number of distinct documents with at least one chunk.
    pub document_count: u64,
}

// ---------------------------------------------------------------------------
// RetrievalStore trait
// ---------------------------------------------------------------------------

/// The storage abstraction for a single knowledge base.
///
/// Production storage is implemented by `store-libsql`.
///
/// This trait is object-safe and `Send + Sync` so it can be boxed and shared across async tasks.
///
/// **Design invariant**: fusion (RRF) is done **above** this trait in `core`, not in the
/// implementations. Each implementation exposes raw ranked lists from each leg.
///
/// See specs/01-architecture.md §4 and specs/04-search-pipeline.md §5.
#[async_trait]
pub trait RetrievalStore: Send + Sync + 'static {
    // ------------------------------------------------------------------
    // Writes (≥90% coverage required)
    // ------------------------------------------------------------------

    /// Upsert a batch of chunk records.
    ///
    /// If a record with the same `id` already exists, it is replaced.
    /// Returns the number of records written (implementations may return the total
    /// count passed in, or only net-new records — callers must not depend on the
    /// exact value for replaced records).
    async fn upsert_chunks(&self, records: Vec<ChunkRecord>) -> Result<usize, Error>;

    /// Delete all chunks belonging to a given document.
    ///
    /// Returns the number of chunks deleted.
    async fn delete_by_resource(&self, resource_id: &str) -> Result<usize, Error>;

    /// Delete all chunks belonging to a given store.
    ///
    /// Used when a store is removed or fully re-indexed.
    async fn delete_by_store(&self, store_id: &str) -> Result<usize, Error>;

    // ------------------------------------------------------------------
    // Reads
    // ------------------------------------------------------------------

    /// Dense vector search.
    ///
    /// Returns up to `limit` results ordered by descending similarity to `query_vector`.
    /// Optional metadata filters are pushed down to the backend.
    async fn dense_search(
        &self,
        query_vector: &[f32],
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error>;

    /// BM25 full-text search.
    ///
    /// Returns up to `limit` results ordered by descending BM25 score.
    /// Optional metadata filters are pushed down to the backend.
    async fn bm25_search(
        &self,
        query_text: &str,
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error>;

    /// Store-level statistics: chunk count, document count.
    async fn stats(&self) -> Result<StoreStats, Error>;

    /// Retrieve a specific chunk by ID. Returns `None` if not found.
    async fn get_chunk(&self, chunk_id: &str) -> Result<Option<ChunkRecord>, Error>;

    /// Retrieve all chunks for a given document.
    async fn get_chunks_for_resource(&self, resource_id: &str) -> Result<Vec<ChunkRecord>, Error>;

    /// Enumerate per-document indexing identity for every distinct document in the
    /// store. Used to rehydrate the incremental-skip index across process runs.
    ///
    /// One record per distinct URI (first chunk wins). Implementations must NOT
    /// return the embedding column to avoid loading vectors for the entire store.
    async fn list_indexed_documents(&self) -> Result<Vec<DocumentRecord>, Error>;

    /// List feed-discovered resources eligible for a liveness probe: rows
    /// owned by `(store_id, source_id)` with `ingestor_kind = "feed"` whose
    /// `last_checked_at` is either unset (never probed) or older than
    /// `checked_before`, ordered oldest first with never-checked rows
    /// leading, capped at `limit`. Backs the feed liveness sweep
    /// (specs/04-search-pipeline.md §1 "Aged-out feed entries: the liveness
    /// sweep"); the sweep itself, including its own guards and the
    /// distinction between "aged out of the window" and "still in it," lives
    /// in `crate::ingestion::run_source_ingestion` — this method is a plain
    /// candidate lookup, not the sweep.
    ///
    /// A plain `ORDER BY last_checked_at ASC` already sorts SQLite `NULL`
    /// before every non-`NULL` value, which is exactly "never-checked
    /// leading" — implementations should rely on that rather than adding a
    /// `CASE`/`COALESCE` to spell it out.
    ///
    /// **Must exclude every URI carrying a fragment.** A link-less feed
    /// entry is stored under a synthetic `{feed_url}#entry:{id}` URI
    /// (specs/02-domain-model.md's "General connector pattern"); HTTP never
    /// sends a fragment on the wire, so probing that URI verbatim would
    /// actually request the feed root, and a 404/410 there would delete the
    /// entry's resource on a signal that has nothing to do with it. This
    /// must be enforced here, not as a post-filter over the returned list —
    /// filtering downstream would leave those rows permanently eligible
    /// (nothing ever advances their `last_checked_at`) and they would keep
    /// occupying `limit` slots forever. The accepted cost — a real entry
    /// link that legitimately carries a fragment is also excluded, and can
    /// never be pruned by this mechanism — is deliberate: deletion here is
    /// asymmetric, so retention bias is the safe failure. See
    /// `store-libsql`'s implementation for the exact SQL.
    ///
    /// The default implementation returns an empty list, mirroring
    /// `upsert_blocks`'s no-op default below: `FakeStore` and any store that
    /// predates the liveness sweep report no candidates, and the sweep
    /// simply has nothing to do for them.
    async fn list_stale_feed_resources(
        &self,
        store_id: &str,
        source_id: &str,
        checked_before: &str,
        limit: usize,
    ) -> Result<Vec<StaleFeedResource>, Error> {
        let _ = (store_id, source_id, checked_before, limit);
        Ok(Vec::new())
    }

    /// Record a liveness probe's outcome for one resource: refresh its
    /// stored conditional-GET validators and `last_checked_at`, and nothing
    /// else.
    ///
    /// **Must never write `index_updated_at`.** That column normatively
    /// means "we last wrote this resource's stored state" and is publicly
    /// exposed as `DocumentInfo::index_updated_at` (`localdb document get`,
    /// `GET /v1/documents/{id}`, MCP `get_document`/`list_documents`). A
    /// liveness probe writes no content and no metadata, so bumping that
    /// column would misreport a merely-pinged resource as re-written — which
    /// is exactly why schema v8 gave the throttle clock its own column
    /// (`last_checked_at`) instead of reusing this one.
    ///
    /// The default implementation is a no-op, mirroring
    /// `list_stale_feed_resources` above.
    async fn touch_resource_liveness(
        &self,
        store_id: &str,
        resource_id: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<(), Error> {
        let _ = (store_id, resource_id, etag, last_modified);
        Ok(())
    }

    /// Update an existing resource's metadata in place, without touching its
    /// chunks, blocks, or embeddings (issue #176's metadata-only incremental
    /// update — specs/04-search-pipeline.md).
    ///
    /// Callers reach this only when `content_hash`/`policy_version` are
    /// unchanged but `core::ids::compute_metadata_hash` differs — a full
    /// reindex (`upsert_chunks_and_blocks`) is the path for everything else.
    /// No default implementation: unlike `upsert_blocks`/
    /// `get_blocks_for_resource`'s no-op defaults (which are legitimately
    /// optional for early/legacy stores), a store that silently accepted
    /// this call and did nothing would report success while the metadata
    /// staleness it was asked to fix persists forever — every implementor
    /// must have an opinion.
    ///
    /// Returns `Err(Error::ResourceNotFound)` if no row matches
    /// `(store_id, resource_id)` — e.g. a concurrent delete raced this
    /// update. Never silently succeeds on zero rows affected: the caller's
    /// `DocumentIndex` entry would otherwise be stamped with a metadata_hash
    /// for a resource_id the store no longer has any row for.
    async fn update_resource_metadata(
        &self,
        store_id: &str,
        resource_id: &str,
        record: &ResourceRecord,
    ) -> Result<(), Error>;

    /// Single-row read of a resource's persisted metadata state — the read
    /// counterpart to [`Self::update_resource_metadata`], returning exactly
    /// the record that method writes.
    ///
    /// This exists because [`Self::get_chunks_for_resource`] cannot stand in
    /// for it. A caller rebuilding a `ResourceRecord` in order to rewrite one
    /// field needs the row's current value for every *other* field, and three
    /// of them — `external_id`, `date_original`, `date_parsed` — are
    /// write-only on `ChunkRecord` by design (see their doc comments): a
    /// chunk read reports `None` for each regardless of what the row holds.
    /// Building a record from a chunk and writing it back therefore nulls
    /// those columns. Widening the chunk projection is not the fix — it also
    /// backs `dense_search`/`bm25_search`, so every search result would carry
    /// and parse fields nothing reads.
    ///
    /// Returns `Ok(None)` when no row matches `(store_id, resource_id)` — a
    /// concurrent delete, or a resource this store never held. No default
    /// implementation, for the same reason `update_resource_metadata` has
    /// none: a store answering `None` unconditionally would silently turn
    /// every caller into a no-op.
    async fn get_resource_record(
        &self,
        store_id: &str,
        resource_id: &str,
    ) -> Result<Option<ResourceRecord>, Error>;

    /// Upsert a set of blocks for a document.
    ///
    /// The resource row identified by `resource_id` must already exist (written
    /// by `upsert_chunks`). The default implementation is a no-op so that
    /// `FakeStore` and test implementations do not need to override it; only
    /// `TenantStore` provides the real persistence.
    async fn upsert_blocks(
        &self,
        store_id: &str,
        resource_id: &str,
        blocks: &[crate::block::Block],
    ) -> Result<(), Error> {
        let _ = (store_id, resource_id, blocks);
        Ok(())
    }

    /// Retrieve all blocks for a document, ordered by `seq`.
    ///
    /// Blocks are the persisted canonical source of truth for document
    /// reconstruction (see `upsert_blocks`): each block's full text is stored
    /// exactly once, unlike chunks, which can duplicate content — most
    /// visibly the table chunker (spec 04 §3, intentional), which re-emits
    /// the header + separator row in every chunk of a multi-chunk table.
    /// Callers reconstructing a document's full text should join these block
    /// texts rather than joining `ChunkRecord.text` across a document's
    /// chunks.
    ///
    /// The default implementation returns an empty vector, mirroring the
    /// default (no-op) `upsert_blocks` above: `FakeStore`-based tests and any
    /// store that never called `upsert_blocks`/`upsert_chunks_and_blocks`
    /// (including legacy rows indexed before the Resource/Block architecture
    /// existed) get `Ok(vec![])` here, not an error. Callers must treat an
    /// empty result as "no blocks persisted for this resource" and fall back
    /// to chunk-based reconstruction accordingly.
    async fn get_blocks_for_resource(
        &self,
        resource_id: &str,
    ) -> Result<Vec<crate::block::Block>, Error> {
        let _ = resource_id;
        Ok(Vec::new())
    }

    /// Atomically upsert chunks and blocks for a document in a single
    /// operation, optionally replacing an existing document first.
    ///
    /// When `replaces_resource_id` is `Some(old_id)`, the old document's
    /// chunks, blocks, and resource row are removed as part of the same
    /// operation, before the new ones are inserted (replace-by-URI
    /// re-indexing; see specs/04-search-pipeline.md §1). Callers performing a
    /// replace must NOT call `delete_by_resource` themselves — passing
    /// `replaces_resource_id` here is the whole point: a write failure must
    /// leave the old document intact and searchable, which is only possible
    /// if the delete and the insert are part of the same operation.
    ///
    /// **The default implementation is NOT atomic.** It performs the delete
    /// (if requested) followed by `upsert_chunks` then `upsert_blocks`,
    /// sequentially, as three separate operations. This is sufficient for
    /// `FakeStore` and unit tests, but a failure partway through can leave
    /// the store in a partially-replaced state. Only the `TenantStore`
    /// (libsql) override wraps the delete and both upserts in a single
    /// database transaction, guaranteeing that a write failure rolls back
    /// the delete along with the insert.
    ///
    /// `external_last_modified` is the resource's raw HTTP `Last-Modified`
    /// conditional-GET validator (`Resource::external_last_modified`), a
    /// trailing parameter rather than a `ChunkRecord` field: unlike
    /// `external_etag`, it is deliberately not denormalized onto every chunk
    /// row (see `ChunkRecord`'s doc comment), since only the owning
    /// resource row needs it. The default implementation below has nowhere
    /// to persist it (no `ChunkRecord`/`upsert_blocks` column carries it) and
    /// ignores it; only `TenantStore` writes it.
    async fn upsert_chunks_and_blocks(
        &self,
        store_id: &str,
        resource_id: &str,
        records: Vec<ChunkRecord>,
        blocks: &[crate::block::Block],
        replaces_resource_id: Option<&str>,
        external_last_modified: Option<&str>,
    ) -> Result<usize, Error> {
        let _ = external_last_modified;
        if let Some(old_id) = replaces_resource_id {
            self.delete_by_resource(old_id).await?;
        }
        let count = self.upsert_chunks(records).await?;
        self.upsert_blocks(store_id, resource_id, blocks).await?;
        Ok(count)
    }
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

/// An in-memory `RetrievalStore` for use in tests.
///
/// No persistence, no actual vector index — linear scan for both legs.
/// Dense search uses cosine similarity; BM25 uses simple term frequency scoring.
#[cfg(any(test, feature = "test-support"))]
pub struct FakeStore {
    chunks: tokio::sync::RwLock<Vec<ChunkRecord>>,
    /// Blocks upserted via `upsert_blocks`/`upsert_chunks_and_blocks`, keyed by
    /// `resource_id` (mirroring `get_chunks_for_resource`'s own
    /// `store_id`-agnostic lookup below — `FakeStore` is used single-store-at-
    /// a-time in tests, so `store_id` is accepted but not partitioned on).
    blocks: tokio::sync::RwLock<HashMap<String, Vec<crate::block::Block>>>,
    /// Every `ResourceRecord` handed to `update_resource_metadata`, in call
    /// order, paired with its `resource_id`.
    ///
    /// `FakeStore` otherwise models a resource's persisted state as the
    /// denormalized fields on its `ChunkRecord`s, which is faithful for every
    /// column `ChunkRecord` carries — but `external_last_modified` is
    /// deliberately not one of them (it is routed through `ResourceRecord`
    /// instead of becoming another per-chunk denormalized copy). Without this
    /// log, a caller's choice of `external_last_modified` would be invisible
    /// to any test using this store, so the preserve-vs-overwrite behavior on
    /// a partially-populated update could not be pinned at all.
    metadata_updates: tokio::sync::RwLock<Vec<(String, ResourceRecord)>>,
}

#[cfg(any(test, feature = "test-support"))]
impl FakeStore {
    /// Create a new empty fake store.
    pub fn new() -> Self {
        Self {
            chunks: tokio::sync::RwLock::new(Vec::new()),
            blocks: tokio::sync::RwLock::new(HashMap::new()),
            metadata_updates: tokio::sync::RwLock::new(Vec::new()),
        }
    }

    /// The `ResourceRecord`s passed to `update_resource_metadata`, in call
    /// order. See the field's own comment for why this log exists.
    pub async fn metadata_updates(&self) -> Vec<(String, ResourceRecord)> {
        self.metadata_updates.read().await.clone()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Default for FakeStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute cosine similarity between two vectors.
#[cfg(any(test, feature = "test-support"))]
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

/// Simple term-frequency BM25 approximation for tests.
///
/// Not a real BM25 implementation — just counts term matches for test purposes.
#[cfg(any(test, feature = "test-support"))]
fn simple_bm25_score(query: &str, text: &str) -> f32 {
    let query_terms: Vec<&str> = query.split_whitespace().collect();
    if query_terms.is_empty() {
        return 0.0;
    }
    let text_lower = text.to_lowercase();
    let matched: usize = query_terms
        .iter()
        .filter(|t| text_lower.contains(&t.to_lowercase()))
        .count();
    matched as f32 / query_terms.len() as f32
}

/// Apply metadata filters to a chunk record. Returns `true` if the record passes.
#[cfg(any(test, feature = "test-support"))]
fn passes_filters(record: &ChunkRecord, filters: &[MetadataFilter]) -> bool {
    filters.iter().all(|f| f.matches(record))
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait]
impl RetrievalStore for FakeStore {
    async fn upsert_chunks(&self, records: Vec<ChunkRecord>) -> Result<usize, Error> {
        let mut chunks = self.chunks.write().await;
        let mut count = 0;
        for record in records {
            if let Some(pos) = chunks.iter().position(|c| c.id == record.id) {
                chunks[pos] = record;
            } else {
                chunks.push(record);
                count += 1;
            }
        }
        Ok(count)
    }

    async fn delete_by_resource(&self, resource_id: &str) -> Result<usize, Error> {
        let mut chunks = self.chunks.write().await;
        let before = chunks.len();
        chunks.retain(|c| c.resource_id != resource_id);
        Ok(before - chunks.len())
    }

    async fn delete_by_store(&self, store_id: &str) -> Result<usize, Error> {
        let mut chunks = self.chunks.write().await;
        let before = chunks.len();
        chunks.retain(|c| c.store_id != store_id);
        Ok(before - chunks.len())
    }

    async fn dense_search(
        &self,
        query_vector: &[f32],
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        let chunks = self.chunks.read().await;
        let mut results: Vec<SearchResult> = chunks
            .iter()
            .filter(|c| passes_filters(c, filters))
            .map(|c| {
                let score = cosine_similarity(query_vector, &c.embedding);
                SearchResult {
                    chunk: c.clone(),
                    score,
                }
            })
            .collect();

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        Ok(results)
    }

    async fn bm25_search(
        &self,
        query_text: &str,
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        let chunks = self.chunks.read().await;
        let mut results: Vec<SearchResult> = chunks
            .iter()
            .filter(|c| passes_filters(c, filters))
            .filter_map(|c| {
                let score = simple_bm25_score(query_text, &c.text);
                if score > 0.0 {
                    Some(SearchResult {
                        chunk: c.clone(),
                        score,
                    })
                } else {
                    None
                }
            })
            .collect();

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        Ok(results)
    }

    async fn stats(&self) -> Result<StoreStats, Error> {
        let chunks = self.chunks.read().await;
        let chunk_count = chunks.len() as u64;
        let doc_ids: std::collections::HashSet<&str> =
            chunks.iter().map(|c| c.resource_id.as_str()).collect();
        Ok(StoreStats {
            chunk_count,
            document_count: doc_ids.len() as u64,
        })
    }

    async fn get_chunk(&self, chunk_id: &str) -> Result<Option<ChunkRecord>, Error> {
        let chunks = self.chunks.read().await;
        Ok(chunks.iter().find(|c| c.id == chunk_id).cloned())
    }

    async fn get_chunks_for_resource(&self, resource_id: &str) -> Result<Vec<ChunkRecord>, Error> {
        let chunks = self.chunks.read().await;
        Ok(chunks
            .iter()
            .filter(|c| c.resource_id == resource_id)
            .cloned()
            .collect())
    }

    async fn list_indexed_documents(&self) -> Result<Vec<DocumentRecord>, Error> {
        let chunks = self.chunks.read().await;
        let mut seen: HashMap<String, DocumentRecord> = HashMap::new();
        for chunk in chunks.iter() {
            seen.entry(chunk.uri.clone()).or_insert(DocumentRecord {
                uri: chunk.uri.clone(),
                resource_id: chunk.resource_id.clone(),
                source_id: chunk.source_id.clone(),
                content_hash: chunk.content_hash.clone(),
                policy_version: chunk.policy_version.clone(),
                // Rehydrated the same way `TenantStore::list_indexed_documents`
                // does: from this chunk's own already-persisted (denormalized)
                // metadata state, not recomputed from some other source of
                // truth — `FakeStore` has no separate `resources` table, so
                // each chunk's fields already *are* that state.
                metadata_hash: crate::ids::compute_metadata_hash(
                    &chunk.metadata,
                    chunk.external_id.as_deref(),
                    chunk.external_etag.as_deref(),
                    chunk.modified_at.as_deref(),
                ),
                external_etag: chunk.external_etag.clone(),
                // `ChunkRecord` deliberately carries no
                // `external_last_modified` (see `upsert_chunks_and_blocks`'s
                // doc comment) — `FakeStore` has nowhere to keep it.
                external_last_modified: None,
            });
        }
        Ok(seen.into_values().collect())
    }

    async fn update_resource_metadata(
        &self,
        store_id: &str,
        resource_id: &str,
        record: &ResourceRecord,
    ) -> Result<(), Error> {
        self.metadata_updates
            .write()
            .await
            .push((resource_id.to_string(), record.clone()));
        let mut chunks = self.chunks.write().await;
        let mut touched = false;
        for chunk in chunks
            .iter_mut()
            .filter(|c| c.store_id == store_id && c.resource_id == resource_id)
        {
            // Mirror the real backend's denormalization: every chunk row for
            // this resource carries its own copy of these fields, so a
            // metadata-only update must touch all of them, exactly as
            // `update_resource_metadata`'s single-row `UPDATE resources ...`
            // does for the real (non-denormalized) `TenantStore`.
            chunk.metadata = record.metadata.clone();
            chunk.external_id = record.external_id.clone();
            chunk.external_etag = record.external_etag.clone();
            chunk.modified_at = record.modified_at.clone();
            chunk.date_original = record.date_original.clone();
            chunk.date_parsed = record.date_parsed.clone();
            touched = true;
        }
        if touched {
            Ok(())
        } else {
            Err(Error::ResourceNotFound {
                id: resource_id.to_string(),
            })
        }
    }

    async fn get_resource_record(
        &self,
        store_id: &str,
        resource_id: &str,
    ) -> Result<Option<ResourceRecord>, Error> {
        let chunks = self.chunks.read().await;
        // `FakeStore` has no separate `resources` table: it denormalizes
        // every resource-level field onto each chunk row, so the first
        // matching chunk *is* the resource's persisted state. That is also
        // why this double cannot reproduce the projection bug the real
        // backend has — see `RetrievalStore::get_resource_record`.
        let Some(chunk) = chunks
            .iter()
            .find(|c| c.store_id == store_id && c.resource_id == resource_id)
        else {
            return Ok(None);
        };
        Ok(Some(ResourceRecord {
            metadata: chunk.metadata.clone(),
            external_id: chunk.external_id.clone(),
            external_etag: chunk.external_etag.clone(),
            // `ChunkRecord` carries no `external_last_modified` (same
            // limitation `list_indexed_documents` above records) — `FakeStore`
            // has nowhere to keep it.
            external_last_modified: None,
            modified_at: chunk.modified_at.clone(),
            date_original: chunk.date_original.clone(),
            date_parsed: chunk.date_parsed.clone(),
        }))
    }

    async fn upsert_blocks(
        &self,
        _store_id: &str,
        resource_id: &str,
        blocks: &[crate::block::Block],
    ) -> Result<(), Error> {
        let mut all_blocks = self.blocks.write().await;
        all_blocks.insert(resource_id.to_string(), blocks.to_vec());
        Ok(())
    }

    async fn get_blocks_for_resource(
        &self,
        resource_id: &str,
    ) -> Result<Vec<crate::block::Block>, Error> {
        let all_blocks = self.blocks.read().await;
        let mut blocks = all_blocks.get(resource_id).cloned().unwrap_or_default();
        blocks.sort_by_key(|b| b.seq);
        Ok(blocks)
    }
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

/// A shared test suite exercising the `RetrievalStore` contract.
///
/// Call this with any concrete implementation. Integration tests in `store-libsql`
/// run this same suite against the real libsql backend.
pub mod conformance {
    use super::*;

    fn make_record(
        id: &str,
        resource_id: &str,
        store_id: &str,
        text: &str,
        embedding: Vec<f32>,
    ) -> ChunkRecord {
        ChunkRecord {
            id: id.to_string(),
            resource_id: resource_id.to_string(),
            store_id: store_id.to_string(),
            text: text.to_string(),
            span: Span::new(0, text.len()),
            heading_path: vec![],
            embedding,
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            modified_at: Some("2026-06-10T12:00:00Z".to_string()),
            content_hash: "abc123".to_string(),
            origin_store: store_id.to_string(),
            source_id: "src-1".to_string(),
            ingestor_kind: "path".to_string(),
            mime: Some("text/plain".to_string()),
            uri: "file:///test.md".to_string(),
            metadata: crate::metadata::Metadata::default(),
            block_seq: 0,
            seq_in_block: 0,
            block_kind: None,
            page: None,
            window_block_seqs: vec![],
            date_original: None,
            date_parsed: None,
            external_id: None,
            external_etag: None,
        }
    }

    /// Test: upsert then stats reflect correct counts.
    pub async fn test_upsert_and_stats(store: &dyn RetrievalStore) {
        let records = vec![
            make_record("chunk-1", "doc-1", "store-1", "Hello world", vec![1.0, 0.0]),
            make_record(
                "chunk-2",
                "doc-1",
                "store-1",
                "Another chunk",
                vec![0.0, 1.0],
            ),
            make_record(
                "chunk-3",
                "doc-2",
                "store-1",
                "Different document",
                vec![0.5, 0.5],
            ),
        ];
        let n = store.upsert_chunks(records).await.unwrap();
        assert_eq!(n, 3, "should upsert 3 new chunks");

        let stats = store.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 3, "chunk_count should be 3");
        assert_eq!(stats.document_count, 2, "document_count should be 2");
    }

    /// Test: upsert replaces existing chunks with the same ID.
    pub async fn test_upsert_replaces_existing(store: &dyn RetrievalStore) {
        let record = make_record(
            "chunk-1",
            "doc-1",
            "store-1",
            "Original text",
            vec![1.0, 0.0],
        );
        store.upsert_chunks(vec![record]).await.unwrap();

        let updated = make_record(
            "chunk-1",
            "doc-1",
            "store-1",
            "Updated text",
            vec![0.5, 0.5],
        );
        let n = store.upsert_chunks(vec![updated]).await.unwrap();
        // Replacement: count may be 0 (no net new chunks)
        let _ = n;

        let chunk = store.get_chunk("chunk-1").await.unwrap();
        assert!(chunk.is_some());
        assert_eq!(chunk.unwrap().text, "Updated text");

        let stats = store.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 1, "should still have exactly 1 chunk");
    }

    /// Test: delete_by_resource removes all chunks for that document.
    pub async fn test_delete_by_resource(store: &dyn RetrievalStore) {
        let records = vec![
            make_record("chunk-1", "doc-1", "store-1", "Doc1 chunk1", vec![1.0, 0.0]),
            make_record("chunk-2", "doc-1", "store-1", "Doc1 chunk2", vec![0.9, 0.1]),
            make_record("chunk-3", "doc-2", "store-1", "Doc2 chunk1", vec![0.0, 1.0]),
        ];
        store.upsert_chunks(records).await.unwrap();

        let deleted = store.delete_by_resource("doc-1").await.unwrap();
        assert_eq!(deleted, 2, "should delete 2 chunks from doc-1");

        let stats = store.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 1, "only doc-2 chunk remains");
        assert_eq!(stats.document_count, 1, "only doc-2 remains");

        // Verify the remaining chunk is from doc-2
        let remaining = store.get_chunks_for_resource("doc-2").await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].resource_id, "doc-2");
    }

    /// Test: delete_by_resource on non-existent document returns 0.
    pub async fn test_delete_nonexistent_document(store: &dyn RetrievalStore) {
        let deleted = store.delete_by_resource("nonexistent-doc").await.unwrap();
        assert_eq!(deleted, 0, "deleting nonexistent doc should return 0");
    }

    /// Test: `upsert_chunks_and_blocks` with `replaces_resource_id` set
    /// deletes the old document and inserts the new one in one call — the
    /// old document's chunks must be gone and only the new document's
    /// chunks remain (issue #79: atomic delete-then-upsert replace).
    pub async fn test_replace_document(store: &dyn RetrievalStore) {
        let old_records = vec![
            make_record(
                "chunk-a1",
                "doc-a",
                "store-1",
                "Doc A chunk 1",
                vec![1.0, 0.0],
            ),
            make_record(
                "chunk-a2",
                "doc-a",
                "store-1",
                "Doc A chunk 2",
                vec![0.9, 0.1],
            ),
        ];
        store.upsert_chunks(old_records).await.unwrap();

        let new_records = vec![make_record(
            "chunk-b1",
            "doc-b",
            "store-1",
            "Doc B chunk 1",
            vec![0.0, 1.0],
        )];
        let written = store
            .upsert_chunks_and_blocks("store-1", "doc-b", new_records, &[], Some("doc-a"), None)
            .await
            .unwrap();
        assert_eq!(written, 1, "should report 1 written chunk for doc-b");

        let doc_a_remaining = store.get_chunks_for_resource("doc-a").await.unwrap();
        assert!(
            doc_a_remaining.is_empty(),
            "doc-a's chunks should be gone after replace"
        );

        let doc_b_remaining = store.get_chunks_for_resource("doc-b").await.unwrap();
        assert_eq!(doc_b_remaining.len(), 1, "doc-b's chunk should be present");

        let stats = store.stats().await.unwrap();
        assert_eq!(
            stats.chunk_count, 1,
            "only doc-b's single chunk should remain"
        );
    }

    /// Test: replacing a document with a new revision that hashes to the
    /// *same* `resource_id` (a policy-only re-index of unchanged content)
    /// deletes then reinserts under the same ID within one call, without
    /// duplicating chunks or violating PK/FK constraints.
    pub async fn test_replace_same_resource_id(store: &dyn RetrievalStore) {
        let old_records = vec![make_record(
            "chunk-1",
            "doc-1",
            "store-1",
            "Original text",
            vec![1.0, 0.0],
        )];
        store.upsert_chunks(old_records).await.unwrap();

        // New revision: different chunk ID (content-addressed), same resource_id.
        let new_records = vec![make_record(
            "chunk-2",
            "doc-1",
            "store-1",
            "Re-chunked text under the same document",
            vec![0.0, 1.0],
        )];
        let written = store
            .upsert_chunks_and_blocks("store-1", "doc-1", new_records, &[], Some("doc-1"), None)
            .await
            .unwrap();
        assert_eq!(written, 1, "should report 1 written chunk");

        let remaining = store.get_chunks_for_resource("doc-1").await.unwrap();
        assert_eq!(
            remaining.len(),
            1,
            "exactly one chunk should remain for doc-1"
        );
        assert_eq!(remaining[0].id, "chunk-2", "old chunk-1 must be gone");

        let stats = store.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 1, "no duplicate chunks after replace");
        assert_eq!(stats.document_count, 1);
    }

    /// Test: dense search returns results ordered by similarity.
    pub async fn test_dense_search_round_trip(store: &dyn RetrievalStore) {
        let records = vec![
            make_record("chunk-1", "doc-1", "store-1", "Close match", vec![1.0, 0.0]),
            make_record(
                "chunk-2",
                "doc-1",
                "store-1",
                "Medium match",
                vec![0.707, 0.707],
            ),
            make_record("chunk-3", "doc-2", "store-1", "Far match", vec![0.0, 1.0]),
        ];
        store.upsert_chunks(records).await.unwrap();

        // Query close to chunk-1
        let results = store.dense_search(&[1.0, 0.0], 3, &[]).await.unwrap();
        assert!(!results.is_empty(), "should return results");
        assert_eq!(
            results[0].chunk.id, "chunk-1",
            "closest chunk should be first"
        );
        assert!(
            results[0].score >= results[1].score,
            "results should be sorted descending by score"
        );
    }

    /// Test: BM25 search returns results containing the query terms.
    pub async fn test_bm25_search_round_trip(store: &dyn RetrievalStore) {
        let records = vec![
            make_record(
                "chunk-1",
                "doc-1",
                "store-1",
                "The quick brown fox jumps",
                vec![1.0, 0.0],
            ),
            make_record(
                "chunk-2",
                "doc-1",
                "store-1",
                "A lazy dog slept",
                vec![0.0, 1.0],
            ),
            make_record(
                "chunk-3",
                "doc-2",
                "store-1",
                "The fox was quick indeed",
                vec![0.5, 0.5],
            ),
        ];
        store.upsert_chunks(records).await.unwrap();

        let results = store.bm25_search("fox quick", 3, &[]).await.unwrap();
        assert!(!results.is_empty(), "BM25 search should find results");
        // Both chunk-1 and chunk-3 contain "fox" and "quick"
        let ids: Vec<&str> = results.iter().map(|r| r.chunk.id.as_str()).collect();
        assert!(
            ids.contains(&"chunk-1") || ids.contains(&"chunk-3"),
            "should find chunks with 'fox' and/or 'quick'"
        );
        // chunk-2 should not appear (no matching terms)
        assert!(
            !ids.contains(&"chunk-2"),
            "lazy dog chunk should not match 'fox quick'"
        );
    }

    /// Test: metadata filter by MIME type.
    pub async fn test_metadata_filter_mime(store: &dyn RetrievalStore) {
        let mut r1 = make_record(
            "chunk-1",
            "doc-1",
            "store-1",
            "markdown doc",
            vec![1.0, 0.0],
        );
        r1.mime = Some("text/markdown".to_string());
        let mut r2 = make_record("chunk-2", "doc-2", "store-1", "html doc", vec![0.5, 0.5]);
        r2.mime = Some("text/html".to_string());

        store.upsert_chunks(vec![r1, r2]).await.unwrap();

        let filter = vec![MetadataFilter::Mime("text/markdown".to_string())];
        let dense_results = store.dense_search(&[1.0, 0.0], 10, &filter).await.unwrap();
        assert_eq!(dense_results.len(), 1, "should only return markdown chunk");
        assert_eq!(dense_results[0].chunk.id, "chunk-1");

        let bm25_results = store.bm25_search("doc", 10, &filter).await.unwrap();
        assert_eq!(bm25_results.len(), 1, "BM25 should also filter by mime");
        assert_eq!(bm25_results[0].chunk.id, "chunk-1");
    }

    /// Test: metadata filter by URI prefix.
    pub async fn test_metadata_filter_uri_prefix(store: &dyn RetrievalStore) {
        let mut r1 = make_record("chunk-1", "doc-1", "store-1", "notes file", vec![1.0, 0.0]);
        r1.uri = "file:///home/user/notes/foo.md".to_string();
        let mut r2 = make_record("chunk-2", "doc-2", "store-1", "docs file", vec![0.5, 0.5]);
        r2.uri = "file:///home/user/docs/bar.md".to_string();

        store.upsert_chunks(vec![r1, r2]).await.unwrap();

        let filter = vec![MetadataFilter::UriPrefix(
            "file:///home/user/notes/".to_string(),
        )];
        let results = store.dense_search(&[1.0, 0.0], 10, &filter).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk.id, "chunk-1");

        // The BM25 leg is a genuine, separate code path from dense_search
        // (its own SQL query in `store-libsql`, its own filter-clause splice
        // point in `FakeStore`) — exercise it too rather than trusting the
        // dense leg's coverage to stand in for it.
        let bm25_results = store.bm25_search("file", 10, &filter).await.unwrap();
        assert_eq!(
            bm25_results.len(),
            1,
            "BM25 should also filter by URI prefix"
        );
        assert_eq!(bm25_results[0].chunk.id, "chunk-1");
    }

    /// Test: metadata filter values are bound as SQL parameters, never
    /// interpolated into query text (issue #255). Table-driven over four
    /// adversarial payloads: a bare single quote (the exact character the
    /// old `'`-doubling escaping handled — direct regression signal), a SQL
    /// line-comment payload, the documented `LIKE`-wildcard character for
    /// `UriPrefix` (specs/04-search-pipeline.md §5), and a non-ASCII
    /// multi-byte string. Each value is embedded in a real resource's
    /// `mime`/`uri` and must be matched (or not) as literal data — the
    /// query must never error, which is the failure mode a reintroduced
    /// interpolation bug would produce.
    pub async fn test_metadata_filter_values_are_bound_not_interpolated(
        store: &dyn RetrievalStore,
    ) {
        let adversarial_values = ["'", "--", "%", "café-日本"];

        for (i, value) in adversarial_values.iter().enumerate() {
            let chunk_id = format!("chunk-adv-{i}");
            let resource_id = format!("doc-adv-{i}");
            let mut matching = make_record(
                &chunk_id,
                &resource_id,
                "store-1",
                "adversarial payload content",
                vec![1.0, 0.0],
            );
            matching.mime = Some(value.to_string());
            matching.uri = format!("file:///adv/{value}/doc.md");

            let other_chunk_id = format!("chunk-adv-other-{i}");
            let other_resource_id = format!("doc-adv-other-{i}");
            let mut other = make_record(
                &other_chunk_id,
                &other_resource_id,
                "store-1",
                "unrelated content",
                vec![0.0, 1.0],
            );
            other.mime = Some("text/plain".to_string());
            other.uri = "file:///unrelated/doc.md".to_string();

            store
                .upsert_chunks(vec![matching, other])
                .await
                .unwrap_or_else(|e| panic!("upsert must not error on {value:?}: {e}"));

            // Mime equality filter: exact match on the adversarial value itself.
            let mime_filter = vec![MetadataFilter::Mime(value.to_string())];
            let dense = store
                .dense_search(&[1.0, 0.0], 10, &mime_filter)
                .await
                .unwrap_or_else(|e| panic!("dense_search must not error on Mime {value:?}: {e}"));
            assert_eq!(
                dense.len(),
                1,
                "Mime filter {value:?} should match exactly the tagged chunk, got {dense:?}"
            );
            assert_eq!(dense[0].chunk.id, chunk_id);

            let bm25 = store
                .bm25_search("adversarial payload content", 10, &mime_filter)
                .await
                .unwrap_or_else(|e| panic!("bm25_search must not error on Mime {value:?}: {e}"));
            assert_eq!(
                bm25.len(),
                1,
                "BM25 with Mime filter {value:?} should match exactly the tagged chunk"
            );
            assert_eq!(bm25[0].chunk.id, chunk_id);

            // UriPrefix filter: the adversarial value sits inside the bound
            // prefix value itself. `%` is the one value expected to behave as
            // a LIKE wildcard here (documented, not a bug); every value must
            // still avoid a SQL error and must still match its own tagged
            // chunk.
            let uri_prefix_filter = vec![MetadataFilter::UriPrefix(format!("file:///adv/{value}"))];
            let uri_results = store
                .dense_search(&[1.0, 0.0], 10, &uri_prefix_filter)
                .await
                .unwrap_or_else(|e| {
                    panic!("dense_search must not error on UriPrefix {value:?}: {e}")
                });
            assert!(
                uri_results.iter().any(|r| r.chunk.id == chunk_id),
                "UriPrefix filter embedding {value:?} should still match its own tagged chunk"
            );

            store.delete_by_resource(&resource_id).await.unwrap();
            store.delete_by_resource(&other_resource_id).await.unwrap();
        }
    }

    /// Test: get_chunk by ID.
    pub async fn test_get_chunk(store: &dyn RetrievalStore) {
        let record = make_record("chunk-1", "doc-1", "store-1", "Hello", vec![1.0, 0.0]);
        store.upsert_chunks(vec![record.clone()]).await.unwrap();

        let found = store.get_chunk("chunk-1").await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, "chunk-1");

        let not_found = store.get_chunk("nonexistent").await.unwrap();
        assert!(not_found.is_none());
    }

    /// Test: get_chunks_for_resource returns all chunks for a document.
    pub async fn test_get_chunks_for_resource(store: &dyn RetrievalStore) {
        let records = vec![
            make_record("chunk-1", "doc-1", "store-1", "First chunk", vec![1.0, 0.0]),
            make_record(
                "chunk-2",
                "doc-1",
                "store-1",
                "Second chunk",
                vec![0.9, 0.1],
            ),
            make_record("chunk-3", "doc-2", "store-1", "Other doc", vec![0.0, 1.0]),
        ];
        store.upsert_chunks(records).await.unwrap();

        let doc1_chunks = store.get_chunks_for_resource("doc-1").await.unwrap();
        assert_eq!(doc1_chunks.len(), 2);

        let doc2_chunks = store.get_chunks_for_resource("doc-2").await.unwrap();
        assert_eq!(doc2_chunks.len(), 1);

        let missing = store.get_chunks_for_resource("nonexistent").await.unwrap();
        assert!(missing.is_empty());
    }

    /// Test: delete_by_store removes all chunks in a store.
    pub async fn test_delete_by_store(store: &dyn RetrievalStore) {
        let records = vec![
            make_record(
                "chunk-1",
                "doc-1",
                "store-A",
                "Store A chunk",
                vec![1.0, 0.0],
            ),
            make_record(
                "chunk-2",
                "doc-2",
                "store-A",
                "Another A chunk",
                vec![0.5, 0.5],
            ),
            make_record(
                "chunk-3",
                "doc-3",
                "store-B",
                "Store B chunk",
                vec![0.0, 1.0],
            ),
        ];
        store.upsert_chunks(records).await.unwrap();

        let deleted = store.delete_by_store("store-A").await.unwrap();
        assert_eq!(deleted, 2, "should delete 2 chunks from store-A");

        let stats = store.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 1);
    }

    /// Test: dense search with limit is respected.
    pub async fn test_dense_search_limit(store: &dyn RetrievalStore) {
        let records: Vec<ChunkRecord> = (0..5)
            .map(|i| {
                make_record(
                    &format!("chunk-{i}"),
                    "doc-1",
                    "store-1",
                    &format!("chunk text {i}"),
                    vec![i as f32 * 0.1, 1.0 - i as f32 * 0.1],
                )
            })
            .collect();
        store.upsert_chunks(records).await.unwrap();

        let results = store.dense_search(&[1.0, 0.0], 2, &[]).await.unwrap();
        assert_eq!(results.len(), 2, "limit should be respected");
    }

    /// Test: BM25 search with limit is respected.
    pub async fn test_bm25_search_limit(store: &dyn RetrievalStore) {
        let records: Vec<ChunkRecord> = (0..5)
            .map(|i| {
                make_record(
                    &format!("chunk-{i}"),
                    "doc-1",
                    "store-1",
                    &format!("search term chunk {i}"),
                    vec![0.5, 0.5],
                )
            })
            .collect();
        store.upsert_chunks(records).await.unwrap();

        let results = store.bm25_search("search term", 2, &[]).await.unwrap();
        assert_eq!(results.len(), 2, "BM25 limit should be respected");
    }

    /// Test: `window_block_seqs` (#129) round-trips through upsert/get.
    ///
    /// A window chunk's non-empty `window_block_seqs` survives write→read intact,
    /// and a plain (non-window) chunk's empty `window_block_seqs` stays empty.
    pub async fn test_window_block_seqs_round_trip(store: &dyn RetrievalStore) {
        let mut windowed = make_record(
            "chunk-window",
            "doc-1",
            "store-1",
            "window chunk text",
            vec![1.0, 0.0],
        );
        windowed.window_block_seqs = vec![3, 4, 5];

        let plain = make_record(
            "chunk-plain",
            "doc-1",
            "store-1",
            "plain chunk text",
            vec![0.0, 1.0],
        );
        assert!(plain.window_block_seqs.is_empty());

        store.upsert_chunks(vec![windowed, plain]).await.unwrap();

        let got_window = store.get_chunk("chunk-window").await.unwrap().unwrap();
        assert_eq!(
            got_window.window_block_seqs,
            vec![3, 4, 5],
            "window chunk's window_block_seqs must survive round trip"
        );

        let got_plain = store.get_chunk("chunk-plain").await.unwrap().unwrap();
        assert!(
            got_plain.window_block_seqs.is_empty(),
            "plain chunk's window_block_seqs must stay empty after round trip"
        );
    }

    /// #103: a chunk's `page` survives the store round trip via the optional
    /// `"page"` key in `location_json`; a chunk without a page reads back
    /// `None` (missing-key compatibility — same pattern as window_block_seqs).
    pub async fn test_page_round_trip(store: &dyn RetrievalStore) {
        let mut paged = make_record(
            "chunk-paged",
            "doc-1",
            "store-1",
            "paged chunk text",
            vec![1.0, 0.0],
        );
        paged.page = Some(7);

        let unpaged = make_record(
            "chunk-unpaged",
            "doc-1",
            "store-1",
            "unpaged chunk text",
            vec![0.0, 1.0],
        );
        assert!(unpaged.page.is_none());

        store.upsert_chunks(vec![paged, unpaged]).await.unwrap();

        let got_paged = store.get_chunk("chunk-paged").await.unwrap().unwrap();
        assert_eq!(
            got_paged.page,
            Some(7),
            "paged chunk's page must survive round trip"
        );

        let got_unpaged = store.get_chunk("chunk-unpaged").await.unwrap().unwrap();
        assert_eq!(
            got_unpaged.page, None,
            "a chunk without a page reads back None (missing-key compat)"
        );
    }

    /// Test: `upsert_blocks` then `get_blocks_for_resource` round-trips
    /// blocks ordered by `seq`, regardless of insertion order — proving
    /// reconstruction can't accidentally depend on physical/insertion order.
    pub async fn test_blocks_round_trip_ordered(store: &dyn RetrievalStore) {
        use crate::block::{Block, BlockKind};

        let chunk = make_record(
            "chunk-1",
            "doc-blocks",
            "store-1",
            "chunk text",
            vec![1.0, 0.0],
        );
        store.upsert_chunks(vec![chunk]).await.unwrap();

        // Insert out of seq order to prove get_blocks_for_resource sorts by
        // seq rather than relying on insertion/physical order.
        let blocks = vec![
            Block {
                seq: 1,
                kind: BlockKind::Text,
                text: "second block".to_string(),
                location: None,
            },
            Block {
                seq: 0,
                kind: BlockKind::Heading { level: 1 },
                text: "first block".to_string(),
                location: None,
            },
        ];
        store
            .upsert_blocks("store-1", "doc-blocks", &blocks)
            .await
            .unwrap();

        let got = store.get_blocks_for_resource("doc-blocks").await.unwrap();
        assert_eq!(got.len(), 2, "both blocks should be returned");
        assert_eq!(got[0].seq, 0, "blocks must be ordered by seq");
        assert_eq!(got[0].text, "first block");
        assert_eq!(got[1].seq, 1);
        assert_eq!(got[1].text, "second block");

        let missing = store.get_blocks_for_resource("nonexistent").await.unwrap();
        assert!(
            missing.is_empty(),
            "unknown resource_id returns empty, not an error"
        );
    }

    /// Test: multiple filters of different kinds combine with AND — a chunk
    /// matching only one of `Mime` and `DateAfter{Added}` must be excluded,
    /// not returned on a partial match.
    pub async fn test_metadata_filter_and_combination(store: &dyn RetrievalStore) {
        let mut both = make_record(
            "chunk-both",
            "doc-both",
            "store-1",
            "matches both filters",
            vec![1.0, 0.0],
        );
        both.mime = Some("text/markdown".to_string());
        both.fetched_at = "2026-06-10T00:00:00Z".to_string();

        let mut mime_only = make_record(
            "chunk-mime-only",
            "doc-mime-only",
            "store-1",
            "right mime, wrong date",
            vec![0.9, 0.1],
        );
        mime_only.mime = Some("text/markdown".to_string());
        mime_only.fetched_at = "2026-01-01T00:00:00Z".to_string();

        let mut date_only = make_record(
            "chunk-date-only",
            "doc-date-only",
            "store-1",
            "right date, wrong mime",
            vec![0.1, 0.9],
        );
        date_only.mime = Some("text/html".to_string());
        date_only.fetched_at = "2026-06-10T00:00:00Z".to_string();

        store
            .upsert_chunks(vec![both, mime_only, date_only])
            .await
            .unwrap();

        let filters = vec![
            MetadataFilter::Mime("text/markdown".to_string()),
            MetadataFilter::DateAfter {
                axis: DateAxis::Added,
                value: "2026-03-01T00:00:00Z".to_string(),
            },
        ];
        let results = store.dense_search(&[1.0, 0.0], 10, &filters).await.unwrap();
        assert_eq!(
            results.len(),
            1,
            "only the chunk matching BOTH filters should be returned, got {results:?}"
        );
        assert_eq!(results[0].chunk.id, "chunk-both");
    }

    /// Test: a chunk with `None` for a nullable date axis (`modified_at`,
    /// `date_parsed`) is excluded by BOTH bound directions, even under a
    /// maximally permissive bound (`"0000"` for `DateAfter`, `"9999"` for
    /// `DateBefore`) that would otherwise trivially satisfy almost any
    /// comparison. Run against the real backend, this is also the test that
    /// proves the SQL `CASE`/`length(NULL)` fall-through in
    /// `DateBefore{Document}` preserves NULL-exclusion rather than
    /// accidentally matching everything.
    ///
    /// `DateAxis::Updated` is deliberately excluded — see `DateAxis::
    /// value_of`'s doc comment: no code path ever produces a NULL there, so
    /// it isn't a reachable state to test.
    pub async fn test_date_filter_null_axis_value_excluded(store: &dyn RetrievalStore) {
        for axis in [DateAxis::Modified, DateAxis::Document] {
            let axis_name = axis.name();
            let id = format!("chunk-{axis_name}-null");
            let resource_id = format!("doc-{axis_name}-null");
            let mut record = make_record(
                &id,
                &resource_id,
                "store-1",
                "no axis value",
                vec![1.0, 0.0],
            );
            match axis {
                DateAxis::Modified => record.modified_at = None,
                DateAxis::Document => record.date_parsed = None,
                DateAxis::Added | DateAxis::Updated => unreachable!("not in this loop's set"),
            }
            store.upsert_chunks(vec![record]).await.unwrap();

            let after = vec![MetadataFilter::DateAfter {
                axis,
                value: "0000".to_string(),
            }];
            let after_results = store.dense_search(&[1.0, 0.0], 10, &after).await.unwrap();
            assert!(
                after_results.is_empty(),
                "{axis_name}: a NULL value must be excluded by DateAfter(\"0000\"), got \
                 {after_results:?}"
            );

            let before = vec![MetadataFilter::DateBefore {
                axis,
                value: "9999".to_string(),
            }];
            let before_results = store.dense_search(&[1.0, 0.0], 10, &before).await.unwrap();
            assert!(
                before_results.is_empty(),
                "{axis_name}: a NULL value must be excluded by DateBefore(\"9999\"), got \
                 {before_results:?}"
            );

            store.delete_by_resource(&resource_id).await.unwrap();
        }
    }

    /// Regression test for the `DateBefore{Document}` widening rule: a chunk
    /// with a bare-year `date_parsed = "2024"` must be
    /// EXCLUDED by `DateBefore{Document, "2024-06-01"}` (its widened latest
    /// instant, December 31st, is later than the bound's widened latest
    /// instant, June 1st), INCLUDED by `DateBefore{Document, "2024-12-31"}`
    /// (both widen to the same day), and INCLUDED by
    /// `DateAfter{Document, "2023-12-31"}` (`DateAfter` needs no widening —
    /// see `core::dates::widen_date_upper_bound`'s doc comment — and `"2024"`
    /// already sorts after `"2023-12-31"` as a plain string).
    /// Test: a partial `DateBefore` bound on a full-timestamp axis must
    /// include the whole period it names, not exclude it.
    ///
    /// The bound is caller-supplied, so it can be partial on any axis, while
    /// `added_at` always holds a full RFC 3339 timestamp. Comparing the two
    /// raw would exclude everything: a longer string sorts after its own
    /// prefix, so `"2026-06-10T12:00:00Z" <= "2026"` is false. Widening the
    /// bound to the latest instant its precision allows is what makes an
    /// inclusive upper bound actually inclusive.
    pub async fn test_date_filter_partial_bound_on_timestamp_axis(store: &dyn RetrievalStore) {
        let mut record = make_record(
            "chunk-added-2026",
            "doc-added-2026",
            "store-1",
            "added mid 2026",
            vec![1.0, 0.0],
        );
        record.fetched_at = "2026-06-10T12:00:00Z".to_string();
        store.upsert_chunks(vec![record]).await.unwrap();

        // Every bound below names a period that CONTAINS the stored instant,
        // so each must match. Before the bound was widened, all three
        // returned nothing.
        for bound in ["2026", "2026-06", "2026-06-10"] {
            let filters = vec![MetadataFilter::DateBefore {
                axis: DateAxis::Added,
                value: bound.to_string(),
            }];
            let results = store.dense_search(&[1.0, 0.0], 10, &filters).await.unwrap();
            assert_eq!(
                results.len(),
                1,
                "DateBefore{{Added, {bound:?}}} must include a resource added \
                 2026-06-10T12:00:00Z — the bound names a period containing it"
            );
        }

        // A bound naming an earlier period must still exclude it, so the
        // widening cannot be over-broad.
        for bound in ["2025", "2026-05", "2026-06-09"] {
            let filters = vec![MetadataFilter::DateBefore {
                axis: DateAxis::Added,
                value: bound.to_string(),
            }];
            let results = store.dense_search(&[1.0, 0.0], 10, &filters).await.unwrap();
            assert!(
                results.is_empty(),
                "DateBefore{{Added, {bound:?}}} must exclude a resource added later"
            );
        }
    }

    pub async fn test_date_filter_document_axis_partial_precision_widening(
        store: &dyn RetrievalStore,
    ) {
        let mut record = make_record(
            "chunk-partial-2024",
            "doc-partial-2024",
            "store-1",
            "bare year dc:date",
            vec![1.0, 0.0],
        );
        record.date_parsed = Some("2024".to_string());
        store.upsert_chunks(vec![record]).await.unwrap();

        let excluding = vec![MetadataFilter::DateBefore {
            axis: DateAxis::Document,
            value: "2024-06-01".to_string(),
        }];
        let excluded = store
            .dense_search(&[1.0, 0.0], 10, &excluding)
            .await
            .unwrap();
        assert!(
            excluded.is_empty(),
            "bare-year 2024 must be EXCLUDED by DateBefore(Document, 2024-06-01), got {excluded:?}"
        );

        let including_before = vec![MetadataFilter::DateBefore {
            axis: DateAxis::Document,
            value: "2024-12-31".to_string(),
        }];
        let included_before = store
            .dense_search(&[1.0, 0.0], 10, &including_before)
            .await
            .unwrap();
        assert_eq!(
            included_before.len(),
            1,
            "bare-year 2024 must be INCLUDED by DateBefore(Document, 2024-12-31), got \
             {included_before:?}"
        );

        let including_after = vec![MetadataFilter::DateAfter {
            axis: DateAxis::Document,
            value: "2023-12-31".to_string(),
        }];
        let included_after = store
            .dense_search(&[1.0, 0.0], 10, &including_after)
            .await
            .unwrap();
        assert_eq!(
            included_after.len(),
            1,
            "bare-year 2024 must be INCLUDED by DateAfter(Document, 2023-12-31), got \
             {included_after:?}"
        );
    }

    /// Table-driven round trip over the three axes that carry a
    /// caller-supplied literal (`Added`, `Modified`, `Document`). Each axis
    /// gets an "old" and a "new" chunk; `DateAfter` on a midpoint bound must
    /// match only the new chunk, `DateBefore` on the same bound only the old
    /// one.
    ///
    /// `DateAxis::Updated` is deliberately excluded from this table — see
    /// `DateAxis::value_of`'s doc comment: the store always stamps its own
    /// write-time clock for that axis, so no fixed literal a test supplies
    /// ever reaches the persisted value, and a uniform fixed-literal
    /// round-trip can't exercise it. See
    /// `test_date_filter_updated_axis_now_relative` (store-libsql-only, using
    /// now-relative bounds) for that axis's own dedicated coverage.
    pub async fn test_date_filter_per_axis_round_trip(store: &dyn RetrievalStore) {
        for axis in [DateAxis::Added, DateAxis::Modified, DateAxis::Document] {
            let axis_name = axis.name();
            let old_id = format!("chunk-{axis_name}-old");
            let new_id = format!("chunk-{axis_name}-new");
            let old_doc = format!("doc-{axis_name}-old");
            let new_doc = format!("doc-{axis_name}-new");

            let mut old = make_record(&old_id, &old_doc, "store-1", "old", vec![1.0, 0.0]);
            let mut new = make_record(&new_id, &new_doc, "store-1", "new", vec![0.5, 0.5]);
            match axis {
                DateAxis::Added => {
                    old.fetched_at = "2026-01-01T00:00:00Z".to_string();
                    new.fetched_at = "2026-06-10T00:00:00Z".to_string();
                }
                DateAxis::Modified => {
                    old.modified_at = Some("2026-01-01T00:00:00Z".to_string());
                    new.modified_at = Some("2026-06-10T00:00:00Z".to_string());
                }
                DateAxis::Document => {
                    old.date_parsed = Some("2026-01-01".to_string());
                    new.date_parsed = Some("2026-06-10".to_string());
                }
                DateAxis::Updated => unreachable!("not in this loop's set"),
            }
            store.upsert_chunks(vec![old, new]).await.unwrap();

            let after = vec![MetadataFilter::DateAfter {
                axis,
                value: "2026-03-01T00:00:00Z".to_string(),
            }];
            let after_results = store.dense_search(&[1.0, 0.0], 10, &after).await.unwrap();
            let after_ids: Vec<&str> = after_results.iter().map(|r| r.chunk.id.as_str()).collect();
            assert_eq!(
                after_ids,
                vec![new_id.as_str()],
                "{axis_name}: DateAfter should match only the new chunk"
            );

            let before = vec![MetadataFilter::DateBefore {
                axis,
                value: "2026-03-01T00:00:00Z".to_string(),
            }];
            let before_results = store.dense_search(&[1.0, 0.0], 10, &before).await.unwrap();
            let before_ids: Vec<&str> =
                before_results.iter().map(|r| r.chunk.id.as_str()).collect();
            assert_eq!(
                before_ids,
                vec![old_id.as_str()],
                "{axis_name}: DateBefore should match only the old chunk"
            );

            // Clean up before the next axis's iteration: `make_record`'s
            // defaults populate every axis's field (e.g. a fixed
            // `modified_at`), so a record left over from this axis could
            // otherwise spuriously match a later axis's filter.
            store.delete_by_resource(&old_doc).await.unwrap();
            store.delete_by_resource(&new_doc).await.unwrap();
        }
    }

    /// Run a subset of the conformance suite that does not require a pre-built FTS index.
    ///
    /// The store must be freshly created (empty) when this is called.
    /// Note: because each conformance function leaves data in the store, this helper
    /// is only useful for backends that can provide a fresh store per call.  For
    /// fine-grained control call each `test_*` function directly (as the per-backend
    /// test modules do).
    ///
    /// Usage: in an async test, create a store, then call `run_non_fts(store).await`.
    pub async fn run_non_fts(store: &dyn RetrievalStore) {
        test_upsert_and_stats(store).await;
    }
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::conformance::*;
    use super::*;

    fn make_test_record(id: &str, doc_id: &str, text: &str, embedding: Vec<f32>) -> ChunkRecord {
        ChunkRecord {
            id: id.to_string(),
            resource_id: doc_id.to_string(),
            store_id: "test-store".to_string(),
            text: text.to_string(),
            span: Span::new(0, text.len()),
            heading_path: vec![],
            embedding,
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            modified_at: Some("2026-06-10T12:00:00Z".to_string()),
            content_hash: "abc123".to_string(),
            origin_store: "test-store".to_string(),
            source_id: "src-1".to_string(),
            ingestor_kind: "path".to_string(),
            mime: Some("text/plain".to_string()),
            uri: "file:///test.md".to_string(),
            metadata: crate::metadata::Metadata::default(),
            block_seq: 0,
            seq_in_block: 0,
            block_kind: None,
            page: None,
            window_block_seqs: vec![],
            date_original: None,
            date_parsed: None,
            external_id: None,
            external_etag: None,
        }
    }

    #[tokio::test]
    async fn fake_store_upsert_and_stats() {
        let store = FakeStore::new();
        test_upsert_and_stats(&store).await;
    }

    #[tokio::test]
    async fn fake_store_upsert_replaces_existing() {
        let store = FakeStore::new();
        test_upsert_replaces_existing(&store).await;
    }

    #[tokio::test]
    async fn fake_store_delete_by_resource() {
        let store = FakeStore::new();
        test_delete_by_resource(&store).await;
    }

    #[tokio::test]
    async fn fake_store_delete_nonexistent_document() {
        let store = FakeStore::new();
        test_delete_nonexistent_document(&store).await;
    }

    #[tokio::test]
    async fn fake_store_replace_document() {
        let store = FakeStore::new();
        test_replace_document(&store).await;
    }

    #[tokio::test]
    async fn fake_store_replace_same_resource_id() {
        let store = FakeStore::new();
        test_replace_same_resource_id(&store).await;
    }

    #[tokio::test]
    async fn fake_store_dense_search_round_trip() {
        let store = FakeStore::new();
        test_dense_search_round_trip(&store).await;
    }

    #[tokio::test]
    async fn fake_store_bm25_search_round_trip() {
        let store = FakeStore::new();
        test_bm25_search_round_trip(&store).await;
    }

    #[tokio::test]
    async fn fake_store_metadata_filter_mime() {
        let store = FakeStore::new();
        test_metadata_filter_mime(&store).await;
    }

    #[tokio::test]
    async fn fake_store_metadata_filter_uri_prefix() {
        let store = FakeStore::new();
        test_metadata_filter_uri_prefix(&store).await;
    }

    #[tokio::test]
    async fn fake_store_metadata_filter_and_combination() {
        let store = FakeStore::new();
        test_metadata_filter_and_combination(&store).await;
    }

    #[tokio::test]
    async fn fake_store_date_filter_null_axis_value_excluded() {
        let store = FakeStore::new();
        test_date_filter_null_axis_value_excluded(&store).await;
    }

    #[tokio::test]
    async fn fake_store_date_filter_partial_bound_on_timestamp_axis() {
        let store = FakeStore::new();
        test_date_filter_partial_bound_on_timestamp_axis(&store).await;
    }

    #[tokio::test]
    async fn fake_store_date_filter_document_axis_partial_precision_widening() {
        let store = FakeStore::new();
        test_date_filter_document_axis_partial_precision_widening(&store).await;
    }

    #[tokio::test]
    async fn fake_store_date_filter_per_axis_round_trip() {
        let store = FakeStore::new();
        test_date_filter_per_axis_round_trip(&store).await;
    }

    #[tokio::test]
    async fn fake_store_get_chunk() {
        let store = FakeStore::new();
        test_get_chunk(&store).await;
    }

    #[tokio::test]
    async fn fake_store_get_chunks_for_resource() {
        let store = FakeStore::new();
        test_get_chunks_for_resource(&store).await;
    }

    #[tokio::test]
    async fn fake_store_delete_by_store() {
        let store = FakeStore::new();
        test_delete_by_store(&store).await;
    }

    #[tokio::test]
    async fn fake_store_dense_search_limit() {
        let store = FakeStore::new();
        test_dense_search_limit(&store).await;
    }

    #[tokio::test]
    async fn fake_store_bm25_search_limit() {
        let store = FakeStore::new();
        test_bm25_search_limit(&store).await;
    }

    #[tokio::test]
    async fn fake_store_window_block_seqs_round_trip() {
        let store = FakeStore::new();
        test_window_block_seqs_round_trip(&store).await;
    }

    #[tokio::test]
    async fn fake_store_page_round_trip() {
        let store = FakeStore::new();
        test_page_round_trip(&store).await;
    }

    #[tokio::test]
    async fn fake_store_blocks_round_trip_ordered() {
        let store = FakeStore::new();
        test_blocks_round_trip_ordered(&store).await;
    }

    #[tokio::test]
    async fn fake_store_empty_stats() {
        let store = FakeStore::new();
        let stats = store.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 0);
        assert_eq!(stats.document_count, 0);
    }

    #[tokio::test]
    async fn fake_store_dense_search_empty() {
        let store = FakeStore::new();
        let results = store.dense_search(&[1.0, 0.0], 10, &[]).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn fake_store_bm25_search_empty() {
        let store = FakeStore::new();
        let results = store.bm25_search("test", 10, &[]).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn fake_store_dense_search_sorted_descending() {
        let store = FakeStore::new();
        let records = vec![
            make_test_record("a", "doc-1", "text a", vec![0.0, 1.0]),
            make_test_record("b", "doc-1", "text b", vec![1.0, 0.0]),
            make_test_record("c", "doc-1", "text c", vec![0.707, 0.707]),
        ];
        store.upsert_chunks(records).await.unwrap();

        let results = store.dense_search(&[1.0, 0.0], 3, &[]).await.unwrap();
        assert_eq!(results.len(), 3);
        // Scores should be descending
        assert!(results[0].score >= results[1].score);
        assert!(results[1].score >= results[2].score);
        // chunk b should be first (closest to [1.0, 0.0])
        assert_eq!(results[0].chunk.id, "b");
    }

    #[tokio::test]
    async fn chunk_record_from_chunk_helper() {
        use crate::types::{Chunk, Provenance, SourceRef};

        let chunk = Chunk {
            id: "chunk-id".to_string(),
            resource_id: "doc-id".to_string(),
            store_id: "store-id".to_string(),
            text: "Some text".to_string(),
            span: Span::new(0, 9),
            heading_path: vec!["Heading".to_string()],
            policy_version: "policy-v1".to_string(),
            provenance: Provenance {
                origin_store: "store-id".to_string(),
                source_ref: SourceRef {
                    id: "source-id".to_string(),
                    kind: "path".to_string(),
                },
                fetched_at: "2026-06-10T12:00:00Z".to_string(),
                content_hash: "abc123".to_string(),
                share_path: vec![],
            },
            window_block_seqs: vec![7, 8],
        };

        let record = ChunkRecord::from_chunk(
            &chunk,
            vec![0.1, 0.2, 0.3],
            "file:///test.md".to_string(),
            Some("text/markdown".to_string()),
            crate::metadata::Metadata::default(),
        );

        assert_eq!(record.id, "chunk-id");
        assert_eq!(record.resource_id, "doc-id");
        assert_eq!(record.store_id, "store-id");
        assert_eq!(record.text, "Some text");
        assert_eq!(record.embedding, vec![0.1, 0.2, 0.3]);
        assert_eq!(record.uri, "file:///test.md");
        assert_eq!(record.mime, Some("text/markdown".to_string()));
        assert_eq!(record.source_id, "source-id");
        assert_eq!(record.ingestor_kind, "path");
        assert_eq!(record.window_block_seqs, vec![7, 8]);
    }

    #[tokio::test]
    async fn cosine_similarity_known_values() {
        // Identical vectors → 1.0
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        // Orthogonal vectors → 0.0
        assert!((cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]) - 0.0).abs() < 1e-6);
        // Zero vector → 0.0
        assert!((cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]) - 0.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn metadata_filter_fetched_after() {
        let store = FakeStore::new();
        let mut r1 = make_test_record("old", "doc-1", "old text", vec![1.0, 0.0]);
        r1.fetched_at = "2026-01-01T00:00:00Z".to_string();
        let mut r2 = make_test_record("new", "doc-2", "new text", vec![0.5, 0.5]);
        r2.fetched_at = "2026-06-10T00:00:00Z".to_string();

        store.upsert_chunks(vec![r1, r2]).await.unwrap();

        let filter = vec![MetadataFilter::DateAfter {
            axis: DateAxis::Added,
            value: "2026-03-01T00:00:00Z".to_string(),
        }];
        let results = store.dense_search(&[1.0, 0.0], 10, &filter).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk.id, "new");
    }

    #[tokio::test]
    async fn metadata_filter_source_id() {
        let store = FakeStore::new();
        let mut r1 = make_test_record("chunk-1", "doc-1", "source A text", vec![1.0, 0.0]);
        r1.source_id = "source-A".to_string();
        let mut r2 = make_test_record("chunk-2", "doc-2", "source B text", vec![0.5, 0.5]);
        r2.source_id = "source-B".to_string();

        store.upsert_chunks(vec![r1, r2]).await.unwrap();

        let filter = vec![MetadataFilter::SourceId("source-A".to_string())];
        let results = store.dense_search(&[1.0, 0.0], 10, &filter).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk.id, "chunk-1");
    }

    #[tokio::test]
    async fn metadata_filter_policy_version() {
        let store = FakeStore::new();
        let mut r1 = make_test_record("chunk-1", "doc-1", "v1 text", vec![1.0, 0.0]);
        r1.policy_version = "policy-v1".to_string();
        let mut r2 = make_test_record("chunk-2", "doc-2", "v2 text", vec![0.5, 0.5]);
        r2.policy_version = "policy-v2".to_string();

        store.upsert_chunks(vec![r1, r2]).await.unwrap();

        let filter = vec![MetadataFilter::PolicyVersion("policy-v1".to_string())];
        let results = store.dense_search(&[1.0, 0.0], 10, &filter).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk.id, "chunk-1");
    }

    #[test]
    fn metadata_filter_matches_all_variants() {
        let record = make_test_record("chunk-1", "doc-1", "text", vec![1.0, 0.0]);

        assert!(MetadataFilter::Mime("text/plain".to_string()).matches(&record));
        assert!(!MetadataFilter::Mime("text/html".to_string()).matches(&record));

        assert!(MetadataFilter::UriPrefix("file:///".to_string()).matches(&record));
        assert!(!MetadataFilter::UriPrefix("https://".to_string()).matches(&record));

        assert!(MetadataFilter::DateAfter {
            axis: DateAxis::Added,
            value: "2026-06-01T00:00:00Z".to_string(),
        }
        .matches(&record));
        assert!(!MetadataFilter::DateAfter {
            axis: DateAxis::Added,
            value: "2026-06-11T00:00:00Z".to_string(),
        }
        .matches(&record));

        assert!(MetadataFilter::DateBefore {
            axis: DateAxis::Added,
            value: "2026-07-01T00:00:00Z".to_string(),
        }
        .matches(&record));
        assert!(!MetadataFilter::DateBefore {
            axis: DateAxis::Added,
            value: "2026-06-01T00:00:00Z".to_string(),
        }
        .matches(&record));

        assert!(MetadataFilter::SourceId("src-1".to_string()).matches(&record));
        assert!(!MetadataFilter::SourceId("src-2".to_string()).matches(&record));

        assert!(MetadataFilter::ResourceId("doc-1".to_string()).matches(&record));
        assert!(!MetadataFilter::ResourceId("doc-2".to_string()).matches(&record));

        assert!(MetadataFilter::PolicyVersion("v1".to_string()).matches(&record));
        assert!(!MetadataFilter::PolicyVersion("v2".to_string()).matches(&record));
    }
}
