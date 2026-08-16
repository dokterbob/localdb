//! Core domain types for localdb.
//!
//! All entities live here. Field lists are normative for meaning.
//! See specs/02-domain-model.md for the full specification.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::ids::{ContentId, UlidId};

// ---------------------------------------------------------------------------
// Span
// ---------------------------------------------------------------------------

/// A byte/char range in the normalized document text.
///
/// Used as citation anchors and chunk boundaries.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Span {
    /// Start offset (byte position) in normalized text.
    pub start: usize,
    /// End offset (byte position) in normalized text.
    pub end: usize,
}

impl Span {
    /// Create a new span with the given start and end offsets.
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

// ---------------------------------------------------------------------------
// Provenance
// ---------------------------------------------------------------------------

/// Provenance metadata carried by every Document and Chunk.
///
/// Chunks must be self-describing for federation — they carry full provenance
/// so their origin can be verified off-node.
///
/// See specs/02-domain-model.md §4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// Store ID where it was first indexed (≠ current store after future federation).
    pub origin_store: UlidId,

    /// Source ID + kind where this content came from.
    pub source_ref: SourceRef,

    /// Acquisition time (file mtime at scan / HTTP fetch time).
    ///
    /// RFC 3339 / ISO 8601 timestamp string.
    pub fetched_at: String,

    /// blake3 hash of normalized content (hex string).
    pub content_hash: String,

    /// Reserved, empty in MVP: list of (node, store) hops for federated content.
    #[serde(default)]
    pub share_path: Vec<FederationHop>,
}

/// Reference to a Source: its ID and kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    /// Source entity ID.
    pub id: UlidId,
    /// Kind of the source (e.g. "path" or "url").
    pub kind: String,
}

/// A single (node, store) hop for federated content. Reserved for future use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FederationHop {
    /// Node identifier.
    pub node: String,
    /// Store identifier.
    pub store: String,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// A named knowledge base.
///
/// Unit of sharing, ACLs, indexing policy, and federation.
///
/// See specs/02-domain-model.md §2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Store {
    /// Stable ULID, minted at creation; never reused.
    pub id: UlidId,

    /// Human-readable name, unique per instance.
    pub name: String,

    /// Visibility: "private" | "shared". MVP: only "private" functional.
    pub visibility: StoreVisibility,

    /// Backend kind + connection info; default "libsql".
    pub backend: BackendConfig,

    /// Indexing policy: `{chunking, embedding}` as one unit.
    pub indexing: IndexingPolicy,

    /// Reserved; empty in MVP.
    #[serde(default)]
    pub acl: Vec<AclEntry>,
}

/// Visibility of a store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoreVisibility {
    Private,
    Shared,
}

/// Backend configuration for a store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackendConfig {
    /// Backend kind, e.g. "libsql".
    pub kind: String,
    /// Connection info, backend-specific.
    #[serde(default)]
    pub connection: HashMap<String, serde_json::Value>,
}

/// Indexing policy for a store: chunking + embedding as one unit.
///
/// Changing either field changes the policy_version hash, which triggers re-index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexingPolicy {
    /// Chunking configuration.
    pub chunking: ChunkingConfig,
    /// Embedding configuration.
    pub embedding: EmbeddingConfig,
}

/// Chunking configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChunkingConfig {
    /// Preset: "prose", "code", or "messages" (reserved).
    pub preset: String,
    /// Optional maximum chunk size in characters.
    #[serde(default)]
    pub max_chars: Option<usize>,
    /// Optional overlap in characters.
    #[serde(default)]
    pub overlap_chars: Option<usize>,
}

/// Embedding configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    /// Provider: "local-onnx", "openai-compatible", etc.
    pub provider: String,
    /// Model name or path.
    pub model: String,
}

/// ACL entry. Reserved; empty in MVP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AclEntry {
    pub principal: String,
    pub role: String,
}

// ---------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------

/// Where a store's content comes from.
///
/// See specs/02-domain-model.md §2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    /// Source ID (ULID).
    pub id: UlidId,

    /// Owning store ID.
    pub store_id: UlidId,

    /// Kind: MVP is "path" | "url". Roadmap: "imap", "mbox".
    pub kind: SourceKind,

    /// Kind-specific spec: root path + globs, or URL + refresh interval.
    pub spec: SourceSpec,

    /// Which indexing preset applies ("prose", "messages", "code").
    pub source_preset: String,
}

/// Source kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Path,
    Url,
    Feed,
}

/// Source spec — kind-specific configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SourceSpec {
    Path {
        /// Root path to index.
        root: String,
        /// Include globs (empty = include all).
        #[serde(default)]
        include: Vec<String>,
        /// Exclude globs.
        #[serde(default)]
        exclude: Vec<String>,
    },
    Url {
        /// The URL to fetch.
        url: String,
        /// Refresh interval in seconds.
        #[serde(default)]
        refresh_interval_secs: Option<u64>,
    },
    Feed {
        /// The Atom/RSS feed URL.
        url: String,
        /// Cap on entries considered per run (`None` = unbounded). A feed
        /// only ever exposes its most recent N entries, so this bounds how
        /// many of those are processed, not how many exist historically.
        #[serde(default)]
        max_entries: Option<u32>,
        /// Whether to follow each entry's link and fetch the full page
        /// content, or index only the feed-supplied summary/content.
        #[serde(default = "default_true")]
        fetch_full_content: bool,
        /// Refresh interval in seconds.
        #[serde(default)]
        refresh_interval_secs: Option<u64>,
    },
}

/// `serde(default = "default_true")` helper: feed sources default to
/// fetching full entry content when `fetch_full_content` is omitted.
fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Document
// ---------------------------------------------------------------------------

/// One logical content unit produced from a source.
///
/// ID is content-addressed: `blake3(canonical_source_uri ‖ content_hash)`.
///
/// See specs/02-domain-model.md §2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    /// Content-addressed ID: `blake3(canonical_source_uri || content_hash)`.
    pub id: ContentId,

    /// Owning source ID.
    pub source_id: UlidId,

    /// Owning store ID.
    pub store_id: UlidId,

    /// Canonical locator (absolute path as `file://`, or URL).
    pub uri: String,

    /// Document title from extraction.
    #[serde(default)]
    pub title: Option<String>,

    /// MIME type from extraction.
    #[serde(default)]
    pub mime: Option<String>,

    /// Language from extraction.
    #[serde(default)]
    pub lang: Option<String>,

    /// blake3 of extracted normalized text (hex). Drives incremental re-index.
    pub content_hash: String,

    /// Provenance metadata.
    pub provenance: Provenance,

    /// Open key-value extension point (string → JSON).
    /// Message fields live here later (reserved `msg.*` keys).
    #[serde(default)]
    pub meta: HashMap<String, serde_json::Value>,
}

impl Document {
    /// Validate `meta` keys: reserved `msg.*` and `dc.*` keys must conform to
    /// the allowed sets.
    ///
    /// Returns `Err` with a description if any reserved key is invalid.
    pub fn validate_meta(&self) -> Result<(), String> {
        for key in self.meta.keys() {
            if key.starts_with("msg.") {
                validate_msg_meta_key(key)?;
            } else if key.starts_with("dc.") {
                validate_dc_meta_key(key)?;
            }
        }
        Ok(())
    }
}

/// Validate a `dc.*` meta key (Dublin Core extension point on `Document.meta`).
///
/// Mirrors the DCMES 1.1 element names. Status: defined and validated, not
/// yet populated in live ingestion (Document isn't constructed in the live path).
pub fn validate_dc_meta_key(key: &str) -> Result<(), String> {
    const ALLOWED_DC_KEYS: &[&str] = &[
        "dc.title",
        "dc.creator",
        "dc.subject",
        "dc.description",
        "dc.publisher",
        "dc.contributor",
        "dc.date",
        "dc.type",
        "dc.format",
        "dc.identifier",
        "dc.source",
        "dc.language",
        "dc.relation",
        "dc.coverage",
        "dc.rights",
    ];
    if ALLOWED_DC_KEYS.contains(&key) {
        Ok(())
    } else {
        Err(format!(
            "unknown reserved meta key '{}'; allowed dc.* keys are: {}",
            key,
            ALLOWED_DC_KEYS.join(", ")
        ))
    }
}

/// Validate a `msg.*` meta key.
///
/// Only reserved keys are allowed:
/// - `msg.thread_id`
/// - `msg.participants`
/// - `msg.sent_at`
/// - `msg.in_reply_to`
/// - `msg.channel`
pub fn validate_msg_meta_key(key: &str) -> Result<(), String> {
    const ALLOWED_MSG_KEYS: &[&str] = &[
        "msg.thread_id",
        "msg.participants",
        "msg.sent_at",
        "msg.in_reply_to",
        "msg.channel",
    ];
    if ALLOWED_MSG_KEYS.contains(&key) {
        Ok(())
    } else {
        Err(format!(
            "unknown reserved meta key '{}'; allowed msg.* keys are: {}",
            key,
            ALLOWED_MSG_KEYS.join(", ")
        ))
    }
}

// ---------------------------------------------------------------------------
// Chunk
// ---------------------------------------------------------------------------

/// The retrieval unit: what gets embedded and indexed.
///
/// ID is content-addressed: `blake3(resource_id || chunk_text || span)`.
///
/// See specs/02-domain-model.md §2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chunk {
    /// Content-addressed ID: `blake3(resource_id || chunk_text || span_start || span_end)`.
    pub id: ContentId,

    /// Parent document ID.
    pub resource_id: ContentId,

    /// Owning store ID.
    pub store_id: UlidId,

    /// Chunk text (also feeds BM25).
    pub text: String,

    /// Range in the normalized document text — the citation anchor.
    pub span: Span,

    /// Heading path inherited from blocks; shown in citations.
    #[serde(default)]
    pub heading_path: Vec<String>,

    /// Hash of the indexing policy that produced this chunk.
    /// See specs/04-search-pipeline.md §4.
    pub policy_version: String,

    /// Provenance copied from document.
    /// Chunks must be self-describing for federation.
    pub provenance: Provenance,

    /// For message-window chunks: all block seqs participating in the window.
    /// Empty for non-window chunks (the common case).
    #[serde(default)]
    pub window_block_seqs: Vec<u32>,
}

// ---------------------------------------------------------------------------
// IndexJob
// ---------------------------------------------------------------------------

/// A unit of indexing work with observable state.
///
/// Embedded mode runs jobs synchronously but still records them;
/// the daemon queues them.
///
/// See specs/02-domain-model.md §2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexJob {
    /// Job ID (ULID).
    pub id: UlidId,

    /// Owning store ID.
    pub store_id: UlidId,

    /// Scope of the indexing work.
    pub scope: IndexJobScope,

    /// Current state of the job.
    pub state: IndexJobState,

    /// Job statistics.
    pub stats: IndexJobStats,

    /// Error message if state is `Failed`.
    #[serde(default)]
    pub error: Option<String>,

    /// Stable machine-readable error code (`Error::code()`) if state is
    /// `Failed` and the failure came from a typed `core::Error` — `None` for
    /// a synthetic queue-level failure (e.g. "job queue is full or closed",
    /// a task panic) that never had one. `#[serde(default)]` so an older
    /// daemon's job JSON (predating this field) still deserializes.
    /// `cli::job_attach::finish_job` reconstructs the original typed error
    /// via `Error::from_code(error_code, error)` so a daemon-attached job
    /// failure exits with the same code an embedded pre-flight failure of
    /// the same kind would (issue #187 review).
    #[serde(default)]
    pub error_code: Option<String>,

    /// When the job was created (RFC 3339).
    pub created_at: String,

    /// When the job started running (RFC 3339), if it has. `None` for a
    /// still-`Pending` job, and also stays `None` on a job cancelled while
    /// still `Pending` (see [`IndexJobState`]'s doc comment) even though
    /// that job is terminal — `completed_at.is_some()` does not imply
    /// `started_at.is_some()`.
    #[serde(default)]
    pub started_at: Option<String>,

    /// When the job completed (RFC 3339), if it has.
    #[serde(default)]
    pub completed_at: Option<String>,
}

/// Scope of an index job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IndexJobScope {
    /// Full store re-index.
    Store,
    /// One source.
    Source { source_id: UlidId },
    /// One document.
    Document { resource_id: ContentId },
}

/// State of an index job: pending → running → done | failed.
///
/// One edge is not on that spine: `pending → failed` directly, with
/// `error_code: "job_cancelled"` (issue #218) — an operator can cancel a job
/// that is still queued, before the worker ever starts it. That job's
/// `started_at` stays `None` (it never ran) while `completed_at` is set (it
/// did reach a terminal state) — the only path that produces that
/// particular pair. Every other terminal job has `started_at: Some(_)`,
/// since `running → done`/`running → failed` are the only other ways to
/// reach a terminal state. A consumer must not assume `completed_at.is_some()
/// implies started_at.is_some()` (e.g. when computing a duration) for
/// exactly this reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexJobState {
    Pending,
    Running,
    Done,
    Failed,
}

/// Statistics accumulated during indexing.
///
/// `#[serde(default)]` at the struct level (issue #187 stage 3): a job
/// producer (or a test fixture) may omit any subset of these fields and
/// still deserialize cleanly, with the omitted fields defaulting to 0 —
/// important now that `IndexJob`s cross the wire (`GET
/// /v1/jobs/{id}/events`'s terminal `job` frame) to a CLI that only cares
/// about a few of them in most cases.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexJobStats {
    /// Documents seen in the scan.
    pub docs_seen: u64,
    /// Documents actually indexed (new or changed).
    pub docs_indexed: u64,
    /// Documents skipped (unchanged content hash).
    pub docs_skipped: u64,
    /// Documents deleted (source removed them).
    pub docs_deleted: u64,
    /// Documents that would have been deleted had deletion been enabled
    /// (`DeletionPolicy::Retain` ran instead of `Prune`) — mirrors
    /// `IngestionResult::docs_prunable`, surfaced here so a daemon-submitted
    /// job can report the same "N no longer at source (kept; use --delete to
    /// remove)" information the embedded CLI path already does.
    pub docs_prunable: u64,
    /// Chunks written to the retrieval backend.
    pub chunks_written: u64,
    /// Files that could not be indexed due to unsupported format.
    pub unsupported_format_count: u64,
    /// Files that errored during indexing.
    pub error_count: u64,
    /// Number of sources the job's scope resolved to, before any were
    /// processed. Distinguishes "this store/source had nothing to index" (0)
    /// from "sources existed but nothing needed indexing" (>0, all other
    /// counters 0) — the same distinction `IndexSummary::has_sources` makes
    /// on the CLI side (issue #187 stage 3).
    pub sources_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{chunk_id, content_hash, new_ulid, resource_id};

    fn make_provenance() -> Provenance {
        Provenance {
            origin_store: new_ulid(),
            source_ref: SourceRef {
                id: new_ulid(),
                kind: "path".to_string(),
            },
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: content_hash("test content"),
            share_path: vec![],
        }
    }

    // --- Provenance tests ---

    #[test]
    fn provenance_serializes_roundtrip() {
        let p = make_provenance();
        let json = serde_json::to_string(&p).unwrap();
        let p2: Provenance = serde_json::from_str(&json).unwrap();
        assert_eq!(p, p2);
    }

    #[test]
    fn provenance_share_path_defaults_empty() {
        let json = r#"{
            "origin_store": "01HN1Y28MYWN6X5DSKZMNE1T5W",
            "source_ref": {"id": "01HN1Y28MYWN6X5DSKZMNE1T5X", "kind": "path"},
            "fetched_at": "2026-06-10T12:00:00Z",
            "content_hash": "abc123"
        }"#;
        let p: Provenance = serde_json::from_str(json).unwrap();
        assert!(p.share_path.is_empty());
    }

    // --- Store tests ---

    #[test]
    fn store_serializes_roundtrip() {
        let store = Store {
            id: new_ulid(),
            name: "test-store".to_string(),
            visibility: StoreVisibility::Private,
            backend: BackendConfig {
                kind: "libsql".to_string(),
                connection: HashMap::new(),
            },
            indexing: IndexingPolicy {
                chunking: ChunkingConfig {
                    preset: "prose".to_string(),
                    max_chars: Some(1024),
                    overlap_chars: Some(128),
                },
                embedding: EmbeddingConfig {
                    provider: "local-onnx".to_string(),
                    model: "default".to_string(),
                },
            },
            acl: vec![],
        };
        let json = serde_json::to_string(&store).unwrap();
        let store2: Store = serde_json::from_str(&json).unwrap();
        assert_eq!(store, store2);
    }

    #[test]
    fn store_visibility_serializes_lowercase() {
        let v = StoreVisibility::Private;
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json, serde_json::json!("private"));

        let v2 = StoreVisibility::Shared;
        let json2 = serde_json::to_value(&v2).unwrap();
        assert_eq!(json2, serde_json::json!("shared"));
    }

    // --- Source tests ---

    #[test]
    fn source_serializes_roundtrip() {
        let source = Source {
            id: new_ulid(),
            store_id: new_ulid(),
            kind: SourceKind::Path,
            spec: SourceSpec::Path {
                root: "/home/user/docs".to_string(),
                include: vec!["**/*.md".to_string()],
                exclude: vec![".git/**".to_string()],
            },
            source_preset: "prose".to_string(),
        };
        let json = serde_json::to_string(&source).unwrap();
        let source2: Source = serde_json::from_str(&json).unwrap();
        assert_eq!(source, source2);
    }

    #[test]
    fn url_source_serializes_roundtrip() {
        let source = Source {
            id: new_ulid(),
            store_id: new_ulid(),
            kind: SourceKind::Url,
            spec: SourceSpec::Url {
                url: "https://example.com/docs".to_string(),
                refresh_interval_secs: Some(3600),
            },
            source_preset: "prose".to_string(),
        };
        let json = serde_json::to_string(&source).unwrap();
        let source2: Source = serde_json::from_str(&json).unwrap();
        assert_eq!(source, source2);
    }

    #[test]
    fn feed_source_serializes_roundtrip() {
        let source = Source {
            id: new_ulid(),
            store_id: new_ulid(),
            kind: SourceKind::Feed,
            spec: SourceSpec::Feed {
                url: "https://example.com/feed.xml".to_string(),
                max_entries: Some(50),
                fetch_full_content: false,
                refresh_interval_secs: Some(3600),
            },
            source_preset: "prose".to_string(),
        };
        let json = serde_json::to_string(&source).unwrap();
        let source2: Source = serde_json::from_str(&json).unwrap();
        assert_eq!(source, source2);
    }

    #[test]
    fn feed_source_spec_serializes_with_tag_feed() {
        let spec = SourceSpec::Feed {
            url: "https://example.com/feed.xml".to_string(),
            max_entries: None,
            fetch_full_content: true,
            refresh_interval_secs: None,
        };
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json.get("type").and_then(|v| v.as_str()), Some("feed"));
    }

    #[test]
    fn feed_source_kind_serializes_lowercase() {
        let json = serde_json::to_value(SourceKind::Feed).unwrap();
        assert_eq!(json, serde_json::json!("feed"));
    }

    #[test]
    fn feed_source_spec_omitted_fetch_full_content_defaults_true() {
        let json = r#"{"type": "feed", "url": "https://example.com/feed.xml"}"#;
        let spec: SourceSpec = serde_json::from_str(json).unwrap();
        match spec {
            SourceSpec::Feed {
                fetch_full_content,
                max_entries,
                refresh_interval_secs,
                ..
            } => {
                assert!(
                    fetch_full_content,
                    "fetch_full_content must default to true"
                );
                assert_eq!(max_entries, None);
                assert_eq!(refresh_interval_secs, None);
            }
            _ => panic!("expected feed spec"),
        }
    }

    // --- Document tests ---

    #[test]
    fn document_serializes_roundtrip() {
        let hash = content_hash("some document content");
        let doc = Document {
            id: resource_id("file:///docs/readme.md", &hash),
            source_id: new_ulid(),
            store_id: new_ulid(),
            uri: "file:///docs/readme.md".to_string(),
            title: Some("README".to_string()),
            mime: Some("text/markdown".to_string()),
            lang: Some("en".to_string()),
            content_hash: hash,
            provenance: make_provenance(),
            meta: HashMap::new(),
        };
        let json = serde_json::to_string(&doc).unwrap();
        let doc2: Document = serde_json::from_str(&json).unwrap();
        assert_eq!(doc, doc2);
    }

    #[test]
    fn document_meta_allows_valid_msg_keys() {
        let hash = content_hash("content");
        let mut meta = HashMap::new();
        meta.insert("msg.thread_id".to_string(), serde_json::json!("thread-123"));
        meta.insert(
            "msg.participants".to_string(),
            serde_json::json!(["alice", "bob"]),
        );
        meta.insert(
            "msg.sent_at".to_string(),
            serde_json::json!("2026-06-10T12:00:00Z"),
        );
        meta.insert("msg.in_reply_to".to_string(), serde_json::json!("msg-456"));
        meta.insert("msg.channel".to_string(), serde_json::json!("#general"));

        let doc = Document {
            id: resource_id("imap://acct/folder;uid=1", &hash),
            source_id: new_ulid(),
            store_id: new_ulid(),
            uri: "imap://acct/folder;uid=1".to_string(),
            title: None,
            mime: None,
            lang: None,
            content_hash: hash,
            provenance: make_provenance(),
            meta,
        };
        assert!(
            doc.validate_meta().is_ok(),
            "all reserved msg.* keys should be valid"
        );
    }

    #[test]
    fn document_meta_rejects_unknown_msg_keys() {
        let hash = content_hash("content");
        let mut meta = HashMap::new();
        meta.insert("msg.unknown_key".to_string(), serde_json::json!("value"));

        let doc = Document {
            id: resource_id("file:///test.md", &hash),
            source_id: new_ulid(),
            store_id: new_ulid(),
            uri: "file:///test.md".to_string(),
            title: None,
            mime: None,
            lang: None,
            content_hash: hash,
            provenance: make_provenance(),
            meta,
        };
        assert!(
            doc.validate_meta().is_err(),
            "unknown msg.* key should fail validation"
        );
    }

    #[test]
    fn document_meta_allows_non_msg_keys() {
        let hash = content_hash("content");
        let mut meta = HashMap::new();
        meta.insert("custom.my_key".to_string(), serde_json::json!("value"));
        meta.insert("app_specific".to_string(), serde_json::json!(42));

        let doc = Document {
            id: resource_id("file:///test.md", &hash),
            source_id: new_ulid(),
            store_id: new_ulid(),
            uri: "file:///test.md".to_string(),
            title: None,
            mime: None,
            lang: None,
            content_hash: hash,
            provenance: make_provenance(),
            meta,
        };
        assert!(
            doc.validate_meta().is_ok(),
            "non-msg.* keys should be allowed"
        );
    }

    #[test]
    fn validate_msg_meta_key_accepts_all_reserved_keys() {
        assert!(validate_msg_meta_key("msg.thread_id").is_ok());
        assert!(validate_msg_meta_key("msg.participants").is_ok());
        assert!(validate_msg_meta_key("msg.sent_at").is_ok());
        assert!(validate_msg_meta_key("msg.in_reply_to").is_ok());
        assert!(validate_msg_meta_key("msg.channel").is_ok());
    }

    #[test]
    fn validate_msg_meta_key_rejects_unknown_keys() {
        assert!(validate_msg_meta_key("msg.foo").is_err());
        assert!(validate_msg_meta_key("msg.").is_err());
        assert!(validate_msg_meta_key("msg.THREAD_ID").is_err()); // case sensitive
    }

    // --- Chunk tests ---

    #[test]
    fn chunk_serializes_roundtrip() {
        let doc_id = resource_id("file:///docs/api.md", &content_hash("doc content"));
        let text = "This is a chunk of text.";
        let span = Span::new(0, text.len());
        let id = chunk_id(&doc_id, 0, text, 0);

        let chunk = Chunk {
            id,
            resource_id: doc_id,
            store_id: new_ulid(),
            text: text.to_string(),
            span,
            heading_path: vec!["API".to_string(), "Introduction".to_string()],
            policy_version: "abc123def456".to_string(),
            provenance: make_provenance(),
            window_block_seqs: vec![],
        };
        let json = serde_json::to_string(&chunk).unwrap();
        let chunk2: Chunk = serde_json::from_str(&json).unwrap();
        assert_eq!(chunk, chunk2);
    }

    #[test]
    fn chunk_window_block_seqs_defaults_empty_on_missing_field() {
        // Old JSON, written before window_block_seqs existed, must still deserialize.
        let doc_id = resource_id("file:///docs/api.md", &content_hash("doc content"));
        let text = "This is a chunk of text.";
        let json = format!(
            r#"{{
                "id": "{id}",
                "resource_id": "{doc_id}",
                "store_id": "store-1",
                "text": "{text}",
                "span": {{"start": 0, "end": {len}}},
                "policy_version": "v1",
                "provenance": {{
                    "origin_store": "store-1",
                    "source_ref": {{"id": "src-1", "kind": "path"}},
                    "fetched_at": "2026-06-10T12:00:00Z",
                    "content_hash": "abc123"
                }}
            }}"#,
            id = chunk_id(&doc_id, 0, text, 0),
            len = text.len(),
        );
        let chunk: Chunk = serde_json::from_str(&json).unwrap();
        assert!(chunk.window_block_seqs.is_empty());
    }

    // --- IndexJob tests ---

    #[test]
    fn index_job_serializes_roundtrip() {
        let job = IndexJob {
            id: new_ulid(),
            store_id: new_ulid(),
            scope: IndexJobScope::Store,
            state: IndexJobState::Pending,
            stats: IndexJobStats::default(),
            error: None,
            error_code: None,
            created_at: "2026-06-10T12:00:00Z".to_string(),
            started_at: None,
            completed_at: None,
        };
        let json = serde_json::to_string(&job).unwrap();
        let job2: IndexJob = serde_json::from_str(&json).unwrap();
        assert_eq!(job, job2);
    }

    #[test]
    fn index_job_error_code_defaults_to_none_when_absent_from_json() {
        // A daemon predating this field (issue #187 review) emits `IndexJob`
        // JSON with no `error_code` key at all — `#[serde(default)]` must
        // still deserialize it, not fail the whole response.
        let json = r#"{
            "id": "job-1",
            "store_id": "store-1",
            "scope": {"type": "store"},
            "state": "failed",
            "stats": {},
            "error": "boom",
            "created_at": "2026-06-10T12:00:00Z"
        }"#;
        let job: IndexJob = serde_json::from_str(json).unwrap();
        assert_eq!(job.error_code, None);
        assert_eq!(job.error, Some("boom".to_string()));
    }

    #[test]
    fn index_job_state_transitions() {
        let states = [
            IndexJobState::Pending,
            IndexJobState::Running,
            IndexJobState::Done,
            IndexJobState::Failed,
        ];
        for state in &states {
            let json = serde_json::to_value(state).unwrap();
            let back: IndexJobState = serde_json::from_value(json).unwrap();
            assert_eq!(*state, back);
        }
    }

    #[test]
    fn index_job_state_serializes_lowercase() {
        let s = IndexJobState::Pending;
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            serde_json::json!("pending")
        );
        let s2 = IndexJobState::Done;
        assert_eq!(
            serde_json::to_value(&s2).unwrap(),
            serde_json::json!("done")
        );
    }

    #[test]
    fn index_job_scope_source_roundtrip() {
        let scope = IndexJobScope::Source {
            source_id: new_ulid(),
        };
        let json = serde_json::to_string(&scope).unwrap();
        let scope2: IndexJobScope = serde_json::from_str(&json).unwrap();
        assert_eq!(scope, scope2);
    }

    #[test]
    fn index_job_scope_document_roundtrip() {
        let doc_id = resource_id("file:///test.md", &content_hash("content"));
        let scope = IndexJobScope::Document {
            resource_id: doc_id,
        };
        let json = serde_json::to_string(&scope).unwrap();
        let scope2: IndexJobScope = serde_json::from_str(&json).unwrap();
        assert_eq!(scope, scope2);
    }

    #[test]
    fn index_job_stats_default_all_zero() {
        let stats = IndexJobStats::default();
        assert_eq!(stats.docs_seen, 0);
        assert_eq!(stats.docs_indexed, 0);
        assert_eq!(stats.docs_deleted, 0);
        assert_eq!(stats.chunks_written, 0);
        assert_eq!(stats.unsupported_format_count, 0);
        assert_eq!(stats.error_count, 0);
    }

    // --- Span tests ---

    #[test]
    fn span_new_and_serialization() {
        let span = Span::new(10, 25);
        assert_eq!(span.start, 10);
        assert_eq!(span.end, 25);
        let json = serde_json::to_string(&span).unwrap();
        let span2: Span = serde_json::from_str(&json).unwrap();
        assert_eq!(span, span2);
    }
}
