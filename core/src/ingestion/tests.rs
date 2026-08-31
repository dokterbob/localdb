use super::*;
use crate::block::Resource;
use crate::embedder::{DocumentChunks, Embedder, FakeEmbedder};
use crate::ids::content_hash;
use crate::ids::resource_id;
use crate::ingestion::enumerate::glob_match;
use crate::ingestion::liveness::{FEED_LIVENESS_BATCH_LIMIT, FEED_LIVENESS_OVERFETCH_CAP};
use crate::ingestion::pipeline::{effective_chunker_config, scale_to_chars};
use crate::ingestor::{IngestCallback, MetadataWriteOutcome, SkipReason};
use crate::store::{ChunkRecord, FakeStore, StaleFeedResource};
use crate::types::{SourceKind, SourceSpec};

fn make_ingestion_config(store_id: &str) -> IngestionConfig {
    IngestionConfig {
        store_id: store_id.to_string(),
        policy_version: "policy-v1".to_string(),
        chunker: ChunkerConfig::prose(),
    }
}

// ---------------------------------------------------------------------------
// DocumentIndex tests
// ---------------------------------------------------------------------------

#[test]
fn document_index_empty() {
    let idx = DocumentIndex::new();
    assert!(idx.is_empty());
    assert_eq!(idx.len(), 0);
}

#[test]
fn document_index_upsert_and_get() {
    let mut idx = DocumentIndex::new();
    let rec = DocumentRecord {
        uri: "file:///test.md".to_string(),
        resource_id: "doc-id-1".to_string(),
        source_id: "src-1".to_string(),
        content_hash: "hash-1".to_string(),
        policy_version: "v1".to_string(),
        metadata_hash: "mhash-1".to_string(),
        external_etag: None,
        external_last_modified: None,
    };
    idx.upsert(rec.clone());
    let found = idx.get("file:///test.md").unwrap();
    assert_eq!(found.resource_id, "doc-id-1");
}

#[test]
fn document_index_remove() {
    let mut idx = DocumentIndex::new();
    let rec = DocumentRecord {
        uri: "file:///test.md".to_string(),
        resource_id: "doc-id-1".to_string(),
        source_id: "src-1".to_string(),
        content_hash: "hash-1".to_string(),
        policy_version: "v1".to_string(),
        metadata_hash: "mhash-1".to_string(),
        external_etag: None,
        external_last_modified: None,
    };
    idx.upsert(rec);
    let removed = idx.remove("file:///test.md");
    assert!(removed.is_some());
    assert!(idx.is_empty());
}

// ---------------------------------------------------------------------------
// IngestionResult wire compatibility
// ---------------------------------------------------------------------------

/// This type crosses a version boundary: `localdb index` attaches to a
/// running daemon's SSE stream, and the two are not upgraded together.
/// Without struct-level `#[serde(default)]` a newer CLI reading an older
/// daemon's `SourceFinished` frame fails the whole deserialize on the
/// first field the daemon never sent, dropping a frame the user is
/// watching.
///
/// Asserted by deserializing an *empty* object, not by round-tripping a
/// populated one — a round trip passes with or without the attribute,
/// since it never omits a field.
#[test]
fn ingestion_result_deserializes_from_an_empty_object() {
    let from_nothing: IngestionResult =
        serde_json::from_str("{}").expect("every field must be optional on the wire");
    let expected = IngestionResult::default();
    assert_eq!(from_nothing.docs_seen, expected.docs_seen);
    assert_eq!(from_nothing.docs_indexed, expected.docs_indexed);
    assert_eq!(from_nothing.docs_skipped, expected.docs_skipped);
    assert_eq!(from_nothing.docs_deleted, expected.docs_deleted);
    assert_eq!(from_nothing.docs_prunable, expected.docs_prunable);
    assert_eq!(
        from_nothing.docs_metadata_updated,
        expected.docs_metadata_updated
    );
    assert_eq!(from_nothing.chunks_written, expected.chunks_written);
    assert_eq!(
        from_nothing.unsupported_format_count,
        expected.unsupported_format_count
    );
    assert_eq!(from_nothing.error_count, expected.error_count);
    assert_eq!(
        from_nothing.document_validators,
        expected.document_validators
    );
    assert_eq!(
        from_nothing.document_inputs_digest,
        expected.document_inputs_digest
    );
}

/// The other direction, which is the one that actually bites in
/// production: an *older* consumer must not choke on a field it has
/// never heard of. Serde ignores unknown keys by default, and nothing
/// on this type opts into `deny_unknown_fields` — pinned here so a
/// future contributor adding it has to argue with a failing test.
#[test]
fn ingestion_result_ignores_fields_it_does_not_know() {
    let from_future: IngestionResult =
        serde_json::from_str(r#"{"docs_seen":3,"docs_teleported":9}"#)
            .expect("an unknown counter must not fail the frame");
    assert_eq!(from_future.docs_seen, 3);
}

// ---------------------------------------------------------------------------
// IndexJob lifecycle tests
// ---------------------------------------------------------------------------

#[test]
fn create_index_job_starts_pending() {
    let job = create_index_job("store-1", IndexJobScope::Store);
    assert_eq!(job.state, IndexJobState::Pending);
    assert!(job.started_at.is_none());
    assert!(job.completed_at.is_none());
    assert!(job.error.is_none());
}

#[test]
fn start_index_job_sets_running() {
    let mut job = create_index_job("store-1", IndexJobScope::Store);
    start_index_job(&mut job);
    assert_eq!(job.state, IndexJobState::Running);
    assert!(job.started_at.is_some());
}

#[test]
fn complete_index_job_sets_done() {
    let mut job = create_index_job("store-1", IndexJobScope::Store);
    start_index_job(&mut job);
    let stats = IndexJobStats {
        docs_seen: 5,
        docs_indexed: 3,
        docs_deleted: 1,
        chunks_written: 12,
        unsupported_format_count: 1,
        error_count: 0,
        ..Default::default()
    };
    complete_index_job(&mut job, stats.clone());
    assert_eq!(job.state, IndexJobState::Done);
    assert!(job.completed_at.is_some());
    assert_eq!(job.stats.docs_seen, 5);
    assert_eq!(job.stats.docs_indexed, 3);
    assert_eq!(job.stats.chunks_written, 12);
}

#[test]
fn fail_index_job_sets_failed() {
    let mut job = create_index_job("store-1", IndexJobScope::Store);
    start_index_job(&mut job);
    fail_index_job(&mut job, "something went wrong".to_string());
    assert_eq!(job.state, IndexJobState::Failed);
    assert_eq!(job.error.as_deref(), Some("something went wrong"));
    assert_eq!(
        job.error_code, None,
        "a synthetic queue-level failure never had a typed error to carry a code from"
    );
    assert!(job.completed_at.is_some());
}

#[test]
fn fail_index_job_with_error_carries_the_typed_errors_code_and_message() {
    let mut job = create_index_job("store-1", IndexJobScope::Store);
    start_index_job(&mut job);
    let err = Error::InvalidConfig {
        message: "unconfigured embedder provider".to_string(),
    };
    fail_index_job_with_error(&mut job, &err);
    assert_eq!(job.state, IndexJobState::Failed);
    // `job.error` must be the *bare* message ("unconfigured embedder
    // provider"), not `err.to_string()` ("invalid config: unconfigured
    // embedder provider"): `cli::job_attach::finish_job` reconstructs the
    // typed error via `Error::from_code(error_code, error)`, which
    // re-adds the "invalid config: " prefix through `Display`. Storing
    // the already-prefixed string here would double it (issue #187
    // review, finding F4).
    assert_eq!(job.error.as_deref(), Some("unconfigured embedder provider"));
    assert_eq!(job.error_code.as_deref(), Some("invalid_config"));
    assert!(job.completed_at.is_some());
}

#[test]
fn fail_index_job_with_error_falls_back_to_display_for_non_reconstructible_variants() {
    // A variant `raw_message()` returns `None` for (e.g. `Internal`,
    // whose fields don't fit a single `message` string) must still
    // populate `job.error` with something readable — the full `Display`
    // string, since there's no bare field to store instead.
    let mut job = create_index_job("store-1", IndexJobScope::Store);
    start_index_job(&mut job);
    let err = Error::Internal {
        message: "bug".to_string(),
        correlation_id: "corr-1".to_string(),
    };
    fail_index_job_with_error(&mut job, &err);
    assert_eq!(job.error.as_deref(), Some(err.to_string().as_str()));
    assert_eq!(job.error_code.as_deref(), Some("internal"));
}

/// Issue #218 review, fix 2: cancelling a still-`Pending` job (before
/// the worker ever calls `start_index_job` on it) goes straight
/// `Pending -> Failed` — the one path that leaves `started_at: None` on
/// a terminal job, since the job never actually ran. Pins the exact
/// record shape `IndexJobState`'s doc comment now documents, produced
/// the same way `server::job_queue::run_worker` produces it for a
/// pending-cancelled job: `fail_index_job_with_error` called on a job
/// that never went through `start_index_job`.
#[test]
fn fail_index_job_with_error_on_a_still_pending_job_leaves_started_at_none() {
    let mut job = create_index_job("store-1", IndexJobScope::Store);
    assert_eq!(job.state, IndexJobState::Pending);
    assert!(job.started_at.is_none());

    fail_index_job_with_error(&mut job, &Error::JobCancelled);

    assert_eq!(job.state, IndexJobState::Failed);
    assert_eq!(job.error_code.as_deref(), Some("job_cancelled"));
    assert!(
        job.started_at.is_none(),
        "a job cancelled before it ever started must not gain a started_at"
    );
    assert!(
        job.completed_at.is_some(),
        "the job is still terminal and must record when that happened"
    );
}

// ---------------------------------------------------------------------------
// glob_match tests
// ---------------------------------------------------------------------------

#[test]
fn glob_match_exact() {
    assert!(glob_match("README.md", "README.md"));
    assert!(!glob_match("README.md", "readme.md"));
}

#[test]
fn glob_match_star() {
    assert!(glob_match("*.md", "README.md"));
    assert!(glob_match("*.md", "notes.md"));
    assert!(!glob_match("*.md", "path/to/notes.md")); // * doesn't cross /
}

#[test]
fn glob_match_double_star() {
    assert!(glob_match("**/*.md", "notes.md"));
    assert!(glob_match("**/*.md", "docs/notes.md"));
    assert!(glob_match("**/*.md", "a/b/c/notes.md"));
}

#[test]
fn glob_match_double_star_dir() {
    assert!(glob_match("**/node_modules/**", "a/node_modules/b/c"));
}

#[test]
fn glob_match_question_mark() {
    assert!(glob_match("file?.md", "file1.md"));
    assert!(glob_match("file?.md", "fileA.md"));
    assert!(!glob_match("file?.md", "file10.md"));
}

#[test]
fn glob_match_non_ascii_does_not_panic() {
    // Regression: en-dash (3-byte char) used to land mid-char in `&path[i..]`.
    assert!(glob_match("*.md", "Notes \u{2013} draft.md"));
    assert!(glob_match(
        "**/*.md",
        "caf\u{e9}/r\u{e9}sum\u{e9} \u{2013} v2.md"
    ));
    assert!(glob_match("*", "\u{dc}n\u{ef}c\u{f6}d\u{eb}.txt"));
    assert!(!glob_match("*.pdf", "Notes \u{2013} draft.md"));
}

// ---------------------------------------------------------------------------
// Path source enumeration tests
// ---------------------------------------------------------------------------

/// #156: a root that does not exist is `RootUnavailable`, not an empty
/// `Complete`. Collapsing the two is what let an unmounted volume look
/// like a source whose every file had been deleted.
#[test]
fn enumerate_path_source_missing_root_is_unavailable() {
    let enumeration = enumerate_path_source("/this/path/does/not/exist", &[], &[]).unwrap();
    assert!(
        matches!(enumeration, PathEnumeration::RootUnavailable),
        "a missing root must be reported as unavailable, not as zero files"
    );
}

/// The other half of the distinction: a root that exists and genuinely
/// holds nothing is `Complete(vec![])` — an observation, not an absence
/// of one — and the sweep is right to act on it.
#[test]
fn enumerate_path_source_empty_dir_is_complete_and_empty() {
    let dir = tempfile::tempdir().unwrap();
    let enumeration = enumerate_path_source(dir.path().to_str().unwrap(), &[], &[]).unwrap();
    assert!(
        matches!(&enumeration, PathEnumeration::Complete(files) if files.is_empty()),
        "an existing but empty root is a complete enumeration of zero files"
    );
}

#[test]
fn enumerate_path_source_finds_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.md"), b"# A").unwrap();
    std::fs::write(dir.path().join("b.txt"), b"hello").unwrap();

    let root = dir.path().to_str().unwrap();
    let files = enumerate_path_source(root, &[], &[])
        .unwrap()
        .files()
        .to_vec();
    assert_eq!(files.len(), 2, "should find both files");
}

#[test]
fn enumerate_path_source_include_filter() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.md"), b"# Notes").unwrap();
    std::fs::write(dir.path().join("data.bin"), b"\x00\x01\x02").unwrap();

    let root = dir.path().to_str().unwrap();
    let files = enumerate_path_source(root, &["*.md".to_string()], &[])
        .unwrap()
        .files()
        .to_vec();
    assert_eq!(files.len(), 1, "should find only .md files");
    assert!(files[0].path.to_str().unwrap().ends_with(".md"));
}

#[test]
fn enumerate_path_source_exclude_filter() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("node_modules")).unwrap();
    std::fs::write(dir.path().join("node_modules").join("lib.js"), b"module").unwrap();
    std::fs::write(dir.path().join("app.js"), b"app").unwrap();

    let root = dir.path().to_str().unwrap();
    let files = enumerate_path_source(root, &[], &["**/node_modules/**".to_string()])
        .unwrap()
        .files()
        .to_vec();
    // Should exclude node_modules files
    assert!(
        files
            .iter()
            .all(|f| !f.path.to_str().unwrap().contains("node_modules")),
        "node_modules files should be excluded"
    );
}

#[test]
fn enumerate_excludes_nested_ds_store_by_basename() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("Call")).unwrap();
    std::fs::write(dir.path().join("Call").join(".DS_Store"), b"\x00\x01junk").unwrap();
    std::fs::write(dir.path().join("Call").join("note.md"), b"# Note").unwrap();
    std::fs::write(dir.path().join(".DS_Store"), b"\x00root").unwrap();

    let root = dir.path().to_str().unwrap();
    let files = enumerate_path_source(root, &[], &[".DS_Store".to_string()])
        .unwrap()
        .files()
        .to_vec();
    assert!(
        files
            .iter()
            .all(|f| !f.path.to_string_lossy().ends_with(".DS_Store")),
        "no .DS_Store at any depth should be enumerated"
    );
    assert!(files
        .iter()
        .any(|f| f.path.to_string_lossy().ends_with("note.md")));
}

#[test]
fn enumerate_prunes_nested_junk_dirs_by_basename() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("a").join(".git")).unwrap();
    std::fs::write(dir.path().join("a").join(".git").join("config"), b"x").unwrap();
    std::fs::create_dir_all(dir.path().join("a").join("node_modules").join("pkg")).unwrap();
    std::fs::write(
        dir.path()
            .join("a")
            .join("node_modules")
            .join("pkg")
            .join("i.js"),
        b"j",
    )
    .unwrap();
    std::fs::write(dir.path().join("a").join("keep.md"), b"# Keep").unwrap();

    let root = dir.path().to_str().unwrap();
    let files = enumerate_path_source(root, &[], &[".git".to_string(), "node_modules".to_string()])
        .unwrap()
        .files()
        .to_vec();
    assert!(
        files.iter().all(|f| {
            let p = f.path.to_string_lossy();
            !p.contains("/.git/") && !p.contains("/node_modules/")
        }),
        "nested .git and node_modules subtrees must be pruned"
    );
    assert_eq!(files.len(), 1);
}

#[test]
fn enumerate_exclude_double_star_pattern_still_works() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub").join(".DS_Store"), b"x").unwrap();
    std::fs::write(dir.path().join("sub").join("a.md"), b"# A").unwrap();

    let root = dir.path().to_str().unwrap();
    let files = enumerate_path_source(root, &[], &["**/.DS_Store".to_string()])
        .unwrap()
        .files()
        .to_vec();
    assert!(files
        .iter()
        .all(|f| !f.path.to_string_lossy().ends_with(".DS_Store")));
}

#[test]
fn enumerate_include_semantics_unchanged_after_exclude_basename_fix() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("docs")).unwrap();
    std::fs::write(dir.path().join("docs").join("notes.md"), b"# N").unwrap();
    std::fs::write(dir.path().join("docs").join("data.bin"), b"\x00").unwrap();

    let root = dir.path().to_str().unwrap();
    // Bare `*.md` include must NOT match nested docs/notes.md (path-anchored).
    let files = enumerate_path_source(root, &["*.md".to_string()], &[])
        .unwrap()
        .files()
        .to_vec();
    assert!(
        files.is_empty(),
        "bare *.md include must not match at depth"
    );
    // `**/*.md` does match.
    let files = enumerate_path_source(root, &["**/*.md".to_string()], &[])
        .unwrap()
        .files()
        .to_vec();
    assert_eq!(files.len(), 1);
    assert!(files[0].path.to_string_lossy().ends_with("notes.md"));
}

#[test]
fn enumerate_exclude_double_star_prunes_nested_dir_before_recursing() {
    // `**/X` (no trailing `/**`) matches the X entry itself, so the dir is
    // excluded before we recurse into it — O(1) prune rather than
    // walk-and-filter. This exercises the shipped DEFAULT_PATH_EXCLUDES form.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("a").join("node_modules").join("big")).unwrap();
    std::fs::write(
        dir.path()
            .join("a")
            .join("node_modules")
            .join("big")
            .join("lib.js"),
        b"module",
    )
    .unwrap();
    std::fs::write(dir.path().join("a").join("keep.rs"), b"fn main() {}").unwrap();

    let root = dir.path().to_str().unwrap();
    let files = enumerate_path_source(root, &[], &["**/node_modules".to_string()])
        .unwrap()
        .files()
        .to_vec();
    assert!(
        files
            .iter()
            .all(|f| !f.path.to_string_lossy().contains("node_modules")),
        "`**/node_modules` must exclude the dir and its contents at any depth"
    );
    assert_eq!(files.len(), 1);
}

#[test]
fn enumerate_path_source_uris_are_file_uris() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("test.md"), b"content").unwrap();

    let root = dir.path().to_str().unwrap();
    let files = enumerate_path_source(root, &[], &[])
        .unwrap()
        .files()
        .to_vec();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].uri.scheme(), "file");
    assert!(files[0].uri.as_str().starts_with("file://"));
}

#[test]
fn enumerate_path_source_handles_non_ascii_filenames() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Notes \u{2013} draft.md"), b"# hi").unwrap();
    std::fs::write(dir.path().join("r\u{e9}sum\u{e9}.txt"), b"x").unwrap();
    let root = dir.path().to_str().unwrap();
    let files = enumerate_path_source(root, &["*.md".to_string()], &[])
        .unwrap()
        .files()
        .to_vec();
    assert_eq!(files.len(), 1); // only the .md, no panic
}

// ---------------------------------------------------------------------------
// A3 — is_store_stale works on an empty FakeStore without panicking
// ---------------------------------------------------------------------------

#[tokio::test]
async fn is_store_stale_empty_store_does_not_panic() {
    let store = FakeStore::new();
    // Must not panic or return an error even though the store is empty.
    let result = is_store_stale(&store, "policy-v1").await;
    assert!(
        result.is_ok(),
        "is_store_stale must not error on empty store"
    );
    assert!(
        !result.unwrap(),
        "empty store must be reported as not stale"
    );
}

#[tokio::test]
async fn store_stale_detection_works() {
    use crate::store::RetrievalStore;

    let store = FakeStore::new();
    let store_id = "store-1";

    // Seed one chunk directly — is_store_stale only samples an existing
    // chunk's policy_version via bm25_search, so there is no need to
    // route this through the ingestion pipeline.
    let mut chunk = make_chunk_record(
        "chunk-1",
        "doc-1",
        store_id,
        "file:///docs/test.md",
        "hash1",
    );
    chunk.policy_version = "policy-v1".to_string();
    store.upsert_chunks(vec![chunk]).await.unwrap();

    // Check with same policy — not stale
    let not_stale = is_store_stale(&store, "policy-v1").await.unwrap();
    assert!(!not_stale, "store should not be stale with same policy");

    // Check with different policy — stale
    let stale = is_store_stale(&store, "policy-v2").await.unwrap();
    assert!(stale, "store should be stale when policy changed");
}

// ---------------------------------------------------------------------------
// A6 / F4 — embed-before-delete ordering and short embedder guard
// ---------------------------------------------------------------------------

/// An embedder that always fails with an internal error.
struct FailingEmbedder;

#[async_trait::async_trait]
impl crate::embedder::Embedder for FailingEmbedder {
    async fn embed_documents(
        &self,
        _docs: Vec<crate::embedder::DocumentChunks>,
    ) -> Result<Vec<crate::embedder::EmbeddedDocument>, Error> {
        Err(Error::Internal {
            message: "intentional embedder failure for testing".to_string(),
            correlation_id: "failing_embedder".to_string(),
        })
    }

    fn embedding_dim(&self) -> usize {
        4
    }

    fn model_id(&self) -> &str {
        "failing-embedder"
    }
}

/// An embedder that returns fewer vectors than input chunks.
struct ShortEmbedder {
    dim: usize,
}

#[async_trait::async_trait]
impl crate::embedder::Embedder for ShortEmbedder {
    async fn embed_documents(
        &self,
        docs: Vec<crate::embedder::DocumentChunks>,
    ) -> Result<Vec<crate::embedder::EmbeddedDocument>, Error> {
        // Return one EmbeddedDocument but with fewer vectors than there are chunks.
        let result = docs
            .iter()
            .map(|doc| {
                // Return at most 0 vectors regardless of how many chunks there are.
                let _ = &doc.chunks;
                vec![] // always empty — guarantees a length mismatch
            })
            .collect();
        Ok(result)
    }

    fn embedding_dim(&self) -> usize {
        self.dim
    }

    fn model_id(&self) -> &str {
        "short-embedder"
    }
}

// ---------------------------------------------------------------------------
// scale_to_chars tests
// ---------------------------------------------------------------------------

#[test]
fn scale_to_chars_scales_prose_budget_by_four() {
    let cfg = ChunkerConfig {
        preset: "prose".to_string(),
        target_tokens: Some(256),
        overlap_tokens: Some(0),
        window_turns: None,
        stride_turns: None,
    };
    let scaled = scale_to_chars(&cfg);
    assert_eq!(scaled.preset, "prose");
    assert_eq!(
        scaled.resolved_target_tokens(),
        256 * 4,
        "prose target should be scaled ×4 for CharSizer"
    );
    assert_eq!(
        scaled.resolved_overlap_tokens(),
        0,
        "prose overlap should be scaled ×4 for CharSizer (0 × 4 = 0)"
    );
}

#[test]
fn scale_to_chars_does_not_change_code_preset() {
    let cfg = ChunkerConfig {
        preset: "code".to_string(),
        target_tokens: Some(3000),
        overlap_tokens: Some(0),
        window_turns: None,
        stride_turns: None,
    };
    let scaled = scale_to_chars(&cfg);
    assert_eq!(scaled.preset, "code");
    assert_eq!(
        scaled.resolved_target_tokens(),
        3000,
        "code preset must not be scaled"
    );
    assert_eq!(
        scaled.resolved_overlap_tokens(),
        0,
        "code overlap must not be scaled"
    );
}

#[test]
fn scale_to_chars_uses_preset_defaults_when_none() {
    // Verify None values resolve through resolved_* before scaling.
    let cfg = ChunkerConfig {
        preset: "prose".to_string(),
        target_tokens: None,
        overlap_tokens: None,
        window_turns: None,
        stride_turns: None,
    };
    let scaled = scale_to_chars(&cfg);
    // Default prose target is 256; scaled = 256 * 4 = 1024. Overlap 0 → 0.
    assert_eq!(scaled.resolved_target_tokens(), 256 * 4);
    assert_eq!(scaled.resolved_overlap_tokens(), 0);
}

#[tokio::test]
async fn from_records_deduplicates_by_uri() {
    use crate::store::RetrievalStore;

    let store = FakeStore::new();
    // Insert two chunks for the same URI with the same document metadata.
    let chunk_a = make_chunk_record("chunk-1", "doc-1", "store-1", "file:///a.md", "hash1");
    let chunk_b = make_chunk_record("chunk-2", "doc-1", "store-1", "file:///a.md", "hash1");
    let chunk_c = make_chunk_record("chunk-3", "doc-2", "store-1", "file:///b.md", "hash2");
    store
        .upsert_chunks(vec![chunk_a, chunk_b, chunk_c])
        .await
        .unwrap();

    let records = store.list_indexed_documents().await.unwrap();
    assert_eq!(records.len(), 2, "two distinct URIs → two records");

    let idx = DocumentIndex::from_records(records);
    assert_eq!(idx.len(), 2);
    assert!(idx.get("file:///a.md").is_some());
    assert!(idx.get("file:///b.md").is_some());
}

fn make_chunk_record(
    id: &str,
    doc_id: &str,
    store_id: &str,
    uri: &str,
    content_hash: &str,
) -> crate::store::ChunkRecord {
    use crate::types::Span;
    crate::store::ChunkRecord {
        id: id.to_string(),
        resource_id: doc_id.to_string(),
        store_id: store_id.to_string(),
        text: "test text".to_string(),
        span: Span::new(0, 9),
        heading_path: vec![],
        embedding: vec![0.0, 0.0, 0.0, 0.0],
        policy_version: "v1".to_string(),
        fetched_at: "2026-06-22T00:00:00Z".to_string(),
        modified_at: Some("2026-06-22T00:00:00Z".to_string()),
        content_hash: content_hash.to_string(),
        origin_store: store_id.to_string(),
        source_id: "src-1".to_string(),
        ingestor_kind: "path".to_string(),
        mime: None,
        uri: uri.to_string(),
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

// ---------------------------------------------------------------------------
// Pipeline tests — run_source_ingestion / index_resource
//
// Exercises the Ingestor-driven pipeline using a scripted FakeIngestor in
// place of real file/URL enumeration.
// ---------------------------------------------------------------------------
mod unified_pipeline {
    use super::*;
    use crate::block::{Block, BlockKind, IngestorKind, ResourceKind};
    use crate::embedder::EmbeddedDocument;
    use crate::ingestor::IngestResult;
    use crate::metadata::{DocumentMetadata, DublinCoreMetadata, Metadata};
    use crate::progress::{DocOutcome, ProgressEvent};
    use crate::uri::Uri;

    // -----------------------------------------------------------------
    // Log-level capture (suppressed-sweep tests below assert on the
    // actual emitted level, not just on behavior)
    // -----------------------------------------------------------------

    std::thread_local! {
        /// Per-thread capture buffer written by `ThreadLocalCapture`
        /// below. Safe as thread-local rather than shared state because
        /// `#[tokio::test]` (`rt`, no `rt-multi-thread`) keeps an entire
        /// test on the one OS thread that started it — see
        /// `run_capturing_logs`'s doc comment for why this is thread-local
        /// rather than a fresh swapped-in subscriber per test.
        static LOG_CAPTURE_BUF: std::cell::RefCell<Vec<u8>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    /// A `MakeWriter` that always writes to the *current* thread's
    /// `LOG_CAPTURE_BUF`, regardless of which test installed the
    /// subscriber that owns it.
    struct ThreadLocalCapture;

    impl std::io::Write for ThreadLocalCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            LOG_CAPTURE_BUF.with(|b| b.borrow_mut().extend_from_slice(buf));
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ThreadLocalCapture {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            ThreadLocalCapture
        }
    }

    static INIT_LOG_CAPTURE: std::sync::Once = std::sync::Once::new();

    /// Installs one `DEBUG`-level subscriber for the whole test binary,
    /// the first time any test needs to capture logs — deliberately
    /// global (`set_global_default`), never per-test
    /// (`tracing::subscriber::set_default`). `tracing`'s
    /// callsite-interest cache is process-wide, not per-subscriber
    /// (`tracing_core::callsite`): repeatedly registering and dropping a
    /// scoped `Dispatch` per test, under `cargo test`'s default
    /// parallelism, raced that cache badly enough to make `debug!` lines
    /// flakily vanish even with a per-test serialization lock around the
    /// swap — verified reproducible both without a lock and with one.
    /// Installing exactly one `Dispatch` for the process's entire
    /// lifetime removes the register/drop churn that caused it; per-test
    /// isolation instead comes from `LOG_CAPTURE_BUF` above.
    fn ensure_log_capture_installed() {
        INIT_LOG_CAPTURE.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_writer(ThreadLocalCapture)
                .with_ansi(false)
                .with_max_level(tracing::Level::DEBUG)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("installed at most once via std::sync::Once");
        });
    }

    /// Runs `run_source_ingestion`, returning its result alongside every
    /// line the run logged at `DEBUG` or above.
    async fn run_capturing_logs(
        source: &Source,
        ingestor: &dyn Ingestor,
        deps: SourceIngestionDeps<'_>,
    ) -> (Result<IngestionResult, Error>, String) {
        ensure_log_capture_installed();
        LOG_CAPTURE_BUF.with(|b| b.borrow_mut().clear());
        let result = run_source_ingestion(source, ingestor, deps).await;
        let captured = LOG_CAPTURE_BUF.with(|b| String::from_utf8(b.borrow().clone()).unwrap());
        (result, captured)
    }

    // -----------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------

    fn make_source_with_preset(store_id: &str, preset: &str) -> Source {
        Source {
            id: new_ulid(),
            store_id: store_id.to_string(),
            kind: SourceKind::Path,
            spec: SourceSpec::Path {
                root: "/docs".to_string(),
                include: vec![],
                exclude: vec![],
            },
            source_preset: preset.to_string(),
        }
    }

    fn make_resource(uri: &str, text: &str, source_id: &str, store_id: &str) -> Resource {
        make_resource_with_blocks(
            uri,
            source_id,
            store_id,
            vec![Block {
                seq: 0,
                kind: BlockKind::Text,
                text: text.to_string(),
                location: None,
            }],
        )
    }

    fn make_resource_with_blocks(
        uri: &str,
        source_id: &str,
        store_id: &str,
        blocks: Vec<Block>,
    ) -> Resource {
        let joined: String = blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let hash = content_hash(&joined);
        let id = resource_id(uri, &hash);
        Resource {
            id,
            store_id: store_id.to_string(),
            source_id: source_id.to_string(),
            ingestor_kind: IngestorKind::File,
            resource_kind: ResourceKind::Document,
            uri: Uri::parse(uri).unwrap_or_else(|| panic!("invalid test uri: {uri}")),
            external_id: None,
            external_etag: None,
            external_last_modified: None,
            content_hash: hash,
            title: None,
            mime: Some("text/markdown".to_string()),
            metadata: Metadata::Document(DocumentMetadata::default()),
            added_at: "2026-06-10T12:00:00Z".to_string(),
            modified_at: Some("2026-06-10T12:00:00Z".to_string()),
            thread_id: None,
            channel: None,
            participants: vec![],
            origin_store: store_id.to_string(),
            policy_version: "policy-v1".to_string(),
            share_path: None,
            extractor_version: "1".to_string(),
            blocks,
        }
    }

    /// Index a resource directly (bypassing the callback) to seed prior
    /// state in `store/doc_index`, mimicking "already indexed in an
    /// earlier run".
    async fn seed_indexed(
        store: &FakeStore,
        embedder: &FakeEmbedder,
        config: &IngestionConfig,
        source: &Source,
        uri: &str,
        text: &str,
    ) -> DocumentRecord {
        let resource = make_resource(uri, text, &source.id, &config.store_id);
        let deps = IndexResourceDeps {
            store,
            embedder,
            config,
        };
        let outcome = index_resource(&resource, source, None, &deps)
            .await
            .expect("seed index must succeed");
        // Reuse the hash `index_resource` actually persisted rather than
        // recomputing it here — the same "thread it out, don't
        // duplicate" reasoning as `PipelineCallback::on_resource`'s
        // `Written` arm.
        let metadata_hash = match outcome {
            IndexOutcome::Written(_, hash) => hash,
            IndexOutcome::Empty => panic!("seed_indexed: resource must not chunk to empty"),
        };
        // The doc_index key must be the NORMALIZED uri, exactly as
        // `list_indexed_documents` rehydrates it — a raw spelling here
        // diverges from the pipeline's seen-set whenever the path needs
        // percent-encoding (e.g. a directory with a space), and the
        // sweep would delete a live document it just observed.
        DocumentRecord {
            uri: resource.uri.as_str().to_string(),
            resource_id: resource.id.clone(),
            source_id: source.id.clone(),
            content_hash: resource.content_hash.clone(),
            policy_version: config.policy_version.clone(),
            metadata_hash,
            external_etag: resource.external_etag.clone(),
            external_last_modified: resource.external_last_modified.clone(),
        }
    }

    // -----------------------------------------------------------------
    // FakeIngestor — scripted Ingestor for testing run_source_ingestion
    // -----------------------------------------------------------------

    // Test-only fixture enum; the size skew between variants doesn't
    // matter here (small, short-lived Vec<ScriptStep> per test).
    #[allow(clippy::large_enum_variant)]
    enum ScriptStep {
        Discovered(usize),
        Resource(Resource),
        Skipped(String, SkipReason),
        /// Positively confirmed absent at the origin (404/410).
        Gone(String),
    }

    struct FakeIngestor {
        script: std::sync::Mutex<Vec<ScriptStep>>,
        /// What this ingestor claims about enumeration completeness —
        /// `Complete` unless a test is exercising the #156 guard.
        enumeration: Enumeration,
    }

    impl FakeIngestor {
        fn new(script: Vec<ScriptStep>) -> Self {
            Self {
                script: std::sync::Mutex::new(script),
                enumeration: Enumeration::Complete,
            }
        }

        /// An ingestor that ran without error but could not observe the
        /// source — the shape a `FileIngestor` over an unmounted volume
        /// reports.
        fn incomplete(reason: &str) -> Self {
            Self {
                script: std::sync::Mutex::new(vec![]),
                enumeration: Enumeration::Incomplete {
                    reason: reason.to_string(),
                },
            }
        }
    }

    #[async_trait::async_trait]
    impl Ingestor for FakeIngestor {
        fn kind(&self) -> IngestorKind {
            IngestorKind::File
        }

        async fn ingest(
            &self,
            _source: &IngestSource,
            callback: &mut dyn IngestCallback,
        ) -> Result<IngestResult, Error> {
            let steps: Vec<ScriptStep> = std::mem::take(&mut *self.script.lock().unwrap());
            let mut produced = 0;
            let mut skipped = 0;
            let mut errors = 0;
            for step in steps {
                match step {
                    ScriptStep::Discovered(n) => callback.on_discovered(n).await,
                    ScriptStep::Resource(r) => {
                        callback.on_resource(r).await?;
                        produced += 1;
                    }
                    ScriptStep::Skipped(uri, reason) => {
                        // Mirror how a real ingestor bumps its own
                        // `errors` counter in lockstep with every
                        // `on_skipped(SkipReason::Error(_))` call (see
                        // the `run_source_ingestion` debug_assert this
                        // feeds).
                        if matches!(reason, SkipReason::Error(_)) {
                            errors += 1;
                        } else {
                            skipped += 1;
                        }
                        // `on_skipped` now takes an already-canonical
                        // `Uri` (see `Ingestor::on_skipped`'s doc
                        // comment): a real ingestor would build this
                        // from `Uri::parse`/`Uri::from_file_path` itself
                        // before ever reaching the pipeline, so the
                        // fixture does the same rather than accepting a
                        // raw string this trait no longer allows. Every
                        // script in this test module uses a valid
                        // locator, so this `expect` never fires.
                        let uri = Uri::parse(&uri)
                            .unwrap_or_else(|| panic!("invalid test skip uri: {uri}"));
                        callback.on_skipped(&uri, reason).await;
                    }
                    ScriptStep::Gone(uri) => {
                        let uri = Uri::parse(&uri)
                            .unwrap_or_else(|| panic!("invalid test gone uri: {uri}"));
                        callback.on_gone(&uri).await;
                    }
                }
            }
            Ok(IngestResult {
                resources_produced: produced,
                resources_skipped: skipped,
                errors,
                enumeration: self.enumeration.clone(),
                document_validators: None,
            })
        }
    }

    /// Embedder that fails only when a chunk's text contains a marker
    /// substring, delegating to a real `FakeEmbedder` otherwise — lets a
    /// mixed script exercise both a successful resource and a failing one.
    struct SelectiveFailEmbedder {
        fail_marker: &'static str,
        inner: FakeEmbedder,
    }

    #[async_trait::async_trait]
    impl Embedder for SelectiveFailEmbedder {
        async fn embed_documents(
            &self,
            docs: Vec<DocumentChunks>,
        ) -> Result<Vec<EmbeddedDocument>, Error> {
            for doc in &docs {
                if doc.chunks.iter().any(|c| c.contains(self.fail_marker)) {
                    return Err(Error::Internal {
                        message: "selective embedder failure for testing".to_string(),
                        correlation_id: "selective_fail_embedder".to_string(),
                    });
                }
            }
            self.inner.embed_documents(docs).await
        }

        fn embedding_dim(&self) -> usize {
            self.inner.embedding_dim()
        }

        fn model_id(&self) -> &str {
            self.inner.model_id()
        }
    }

    fn progress_collector() -> (
        crate::progress::ProgressSink,
        std::sync::Arc<std::sync::Mutex<Vec<ProgressEvent>>>,
    ) {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let events2 = events.clone();
        let sink: crate::progress::ProgressSink = std::sync::Arc::new(move |e| {
            events2.lock().unwrap().push(e);
        });
        (sink, events)
    }

    // -----------------------------------------------------------------
    // 1. Counter parity for a mixed script
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn counter_parity_for_mixed_script() {
        let store = FakeStore::new();
        let embedder = SelectiveFailEmbedder {
            fail_marker: "FAIL_MARKER",
            inner: FakeEmbedder::new(4),
        };
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");

        let good = make_resource(
            "file:///docs/good.md",
            "Some good content to index.",
            &source.id,
            store_id,
        );
        let bad = make_resource(
            "file:///docs/bad.md",
            "This contains FAIL_MARKER and will error.",
            &source.id,
            store_id,
        );

        let ingestor = FakeIngestor::new(vec![
            ScriptStep::Discovered(4),
            ScriptStep::Resource(good),
            ScriptStep::Resource(bad),
            ScriptStep::Skipped(
                "file:///docs/unchanged.md".to_string(),
                SkipReason::Unchanged,
            ),
            ScriptStep::Skipped(
                "file:///docs/binary.bin".to_string(),
                SkipReason::Unsupported,
            ),
        ]);

        let mut doc_index = DocumentIndex::new();
        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(result.docs_seen, 4, "all four discovered items are seen");
        assert_eq!(result.docs_indexed, 1, "only the good resource indexes");
        assert_eq!(
            result.docs_skipped, 1,
            "on_skipped(Unchanged) counts as skipped"
        );
        assert_eq!(result.unsupported_format_count, 1);
        assert_eq!(
            result.error_count, 1,
            "the failing resource counts as an error"
        );
        assert!(result.chunks_written > 0);
    }

    // -----------------------------------------------------------------
    // 1a. Codex review finding F1 (ingest/url_pipeline.rs) — an
    //     accepted-but-empty extraction reports `SkipReason::Other` and
    //     must land in `docs_skipped`, NOT `unsupported_format_count`:
    //     the two counters mean different things ("extraction produced
    //     nothing" vs "no parser handles this format") and the CLI
    //     reports them as separate fields.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn skip_reason_other_counts_as_docs_skipped_not_unsupported() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");

        let ingestor = FakeIngestor::new(vec![
            ScriptStep::Discovered(1),
            ScriptStep::Skipped(
                "https://example.com/empty".to_string(),
                SkipReason::Other("extraction produced no content".to_string()),
            ),
        ]);

        let mut doc_index = DocumentIndex::new();
        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(
            result.docs_skipped, 1,
            "SkipReason::Other must count as docs_skipped"
        );
        assert_eq!(
            result.unsupported_format_count, 0,
            "SkipReason::Other must NOT count toward unsupported_format_count — \
             that counter is reserved for SkipReason::Unsupported (no parser \
             handles the format), a different condition than an \
             accepted-but-empty extraction"
        );
        assert_eq!(result.error_count, 0);
    }

    // -----------------------------------------------------------------
    // 1b. C8 — SkipReason::Error is counted as an error (not a skip),
    //     while SkipReason::Unchanged still counts as a skip; both keep
    //     their URIs alive across the delete-sweep.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn on_skipped_error_counts_as_error_not_skip_and_survives_sweep() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");

        let error_uri = "file:///docs/transient-failure.md";
        let unchanged_uri = "file:///docs/unchanged.md";

        // Both URIs already have prior indexed content — the run below
        // must leave that content in place (they're reported alive via
        // on_skipped, never seen via on_resource).
        let error_record = seed_indexed(
            &store,
            &embedder,
            &config,
            &source,
            error_uri,
            "Content that will transiently fail this run.",
        )
        .await;
        let unchanged_record = seed_indexed(
            &store,
            &embedder,
            &config,
            &source,
            unchanged_uri,
            "Content that never changes.",
        )
        .await;

        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(error_record.clone());
        doc_index.upsert(unchanged_record.clone());

        let good = make_resource(
            "file:///docs/good.md",
            "Brand new good content.",
            &source.id,
            store_id,
        );

        let ingestor = FakeIngestor::new(vec![
            ScriptStep::Discovered(3),
            ScriptStep::Resource(good),
            ScriptStep::Skipped(
                error_uri.to_string(),
                SkipReason::Error("transient read failure".to_string()),
            ),
            ScriptStep::Skipped(unchanged_uri.to_string(), SkipReason::Unchanged),
        ]);

        let (sink, events) = progress_collector();
        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: Some(sink),
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(result.docs_indexed, 1, "only the new good resource indexes");
        assert_eq!(
            result.docs_skipped, 1,
            "SkipReason::Unchanged still counts as docs_skipped"
        );
        assert_eq!(
            result.error_count, 1,
            "SkipReason::Error must be counted as an error, not a skip"
        );

        // Both previously-indexed URIs must survive the delete-sweep.
        assert!(
            doc_index.get(error_uri).is_some(),
            "the errored URI must stay alive in the doc_index"
        );
        assert!(
            doc_index.get(unchanged_uri).is_some(),
            "the unchanged URI must stay alive in the doc_index"
        );
        assert!(
            !store
                .get_chunks_for_resource(&error_record.resource_id)
                .await
                .unwrap()
                .is_empty(),
            "the errored URI's existing chunks must not be swept"
        );
        assert!(
            !store
                .get_chunks_for_resource(&unchanged_record.resource_id)
                .await
                .unwrap()
                .is_empty(),
            "the unchanged URI's existing chunks must not be swept"
        );

        // Progress event for the errored URI must report DocOutcome::Error,
        // distinct from DocOutcome::Skipped.
        let events = events.lock().unwrap();
        let error_event = events.iter().find_map(|e| match e {
            ProgressEvent::DocumentFinished { uri, outcome } if uri == error_uri => Some(outcome),
            _ => None,
        });
        assert!(
            matches!(error_event, Some(DocOutcome::Error)),
            "expected DocOutcome::Error for the errored URI, got {error_event:?}"
        );
    }

    // -----------------------------------------------------------------
    // 2. Progress-event sequence parity
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn progress_event_sequence_parity() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");

        let good = make_resource(
            "file:///docs/good.md",
            "Some content to index.",
            &source.id,
            store_id,
        );

        let ingestor = FakeIngestor::new(vec![
            ScriptStep::Discovered(2),
            ScriptStep::Resource(good),
            ScriptStep::Skipped(
                "file:///docs/unsupported.bin".to_string(),
                SkipReason::Unsupported,
            ),
        ]);

        let (sink, events) = progress_collector();
        let mut doc_index = DocumentIndex::new();
        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: Some(sink),
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        let events = events.lock().unwrap();
        let kinds: Vec<&'static str> = events
            .iter()
            .map(|e| match e {
                ProgressEvent::SourceStarted { .. } => "source_started",
                ProgressEvent::Discovered { .. } => "discovered",
                ProgressEvent::DocumentStarted { .. } => "doc_started",
                ProgressEvent::DocumentFinished { .. } => "doc_finished",
                ProgressEvent::SourceFinished { .. } => "source_finished",
            })
            .collect();

        assert_eq!(
            kinds,
            vec![
                "source_started",
                "discovered",
                "doc_started",
                "doc_finished",
                "doc_started",
                "doc_finished",
                "source_finished",
            ]
        );

        // The indexed resource must report Indexed{chunks > 0}; the
        // unsupported one must report Unsupported.
        match &events[3] {
            ProgressEvent::DocumentFinished {
                outcome: DocOutcome::Indexed { chunks },
                ..
            } => assert!(*chunks > 0),
            other => panic!("expected Indexed outcome, got {other:?}"),
        }
        match &events[5] {
            ProgressEvent::DocumentFinished {
                outcome: DocOutcome::Unsupported,
                ..
            } => {}
            other => panic!("expected Unsupported outcome, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // 3. Incremental skip via content_hash+policy in the callback
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn callback_skips_unchanged_content_and_policy() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");

        let text = "Stable content that never changes.";
        let uri = "file:///docs/stable.md";
        let record = seed_indexed(&store, &embedder, &config, &source, uri, text).await;
        let chunk_count_before = store.stats().await.unwrap().chunk_count;

        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(record);

        // The ingestor still yields the (unchanged) resource via on_resource —
        // the callback's own skip-check must catch it.
        let resource = make_resource(uri, text, &source.id, store_id);
        let ingestor = FakeIngestor::new(vec![ScriptStep::Resource(resource)]);

        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(result.docs_indexed, 0);
        assert_eq!(result.docs_skipped, 1);
        let chunk_count_after = store.stats().await.unwrap().chunk_count;
        assert_eq!(
            chunk_count_before, chunk_count_after,
            "skip must not write any new chunks"
        );
    }

    // -----------------------------------------------------------------
    // 4. Policy-change forces re-index
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn policy_change_forces_reindex() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config_v1 = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");

        let text = "Content whose policy will change.";
        let uri = "file:///docs/policy.md";
        let record = seed_indexed(&store, &embedder, &config_v1, &source, uri, text).await;
        let old_resource_id = record.resource_id.clone();

        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(record);

        let config_v2 = IngestionConfig {
            store_id: store_id.to_string(),
            policy_version: "policy-v2".to_string(),
            chunker: ChunkerConfig::prose(),
        };

        let resource = make_resource(uri, text, &source.id, store_id);
        let new_resource_id = resource.id.clone();
        let ingestor = FakeIngestor::new(vec![ScriptStep::Resource(resource)]);

        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config_v2,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(
            result.docs_indexed, 1,
            "a policy change must force re-indexing even with unchanged content"
        );

        // Same URI + same content_hash ⇒ same content-addressed resource_id;
        // policy_version isn't a resource_id input, so the id is unchanged,
        // but the chunk's stored policy_version must reflect v2.
        assert_eq!(old_resource_id, new_resource_id);
        let chunks = store
            .get_chunks_for_resource(&new_resource_id)
            .await
            .unwrap();
        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|c| c.policy_version == "policy-v2"));
    }

    // -----------------------------------------------------------------
    // 4b. Cross-process rehydration: DocumentIndex::from_records +
    //     list_indexed_documents skips unchanged and reindexes changed
    //     resources on a simulated second process invocation.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn rehydrated_index_skips_unchanged_and_reindexes_changed() {
        use crate::store::RetrievalStore;

        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");

        let stable_uri = "file:///docs/stable.md";
        let changing_uri = "file:///docs/changing.md";

        // First "process": full index via the scripted ingestor.
        let mut doc_index1 = DocumentIndex::new();
        let ingestor1 = FakeIngestor::new(vec![
            ScriptStep::Resource(make_resource(
                stable_uri,
                "Stable document content.",
                &source.id,
                store_id,
            )),
            ScriptStep::Resource(make_resource(
                changing_uri,
                "Original content.",
                &source.id,
                store_id,
            )),
        ]);
        let deps1 = SourceIngestionDeps {
            doc_index: &mut doc_index1,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result1 = run_source_ingestion(&source, &ingestor1, deps1)
            .await
            .unwrap();
        assert_eq!(result1.docs_indexed, 2);

        // Simulate a new process: rehydrate DocumentIndex from the store
        // rather than reusing the in-memory one from the first run.
        let records = store.list_indexed_documents().await.unwrap();
        assert_eq!(records.len(), 2, "store must have 2 distinct documents");
        let mut doc_index2 = DocumentIndex::from_records(records);

        // Second "process": re-run with one resource changed.
        let ingestor2 = FakeIngestor::new(vec![
            ScriptStep::Resource(make_resource(
                stable_uri,
                "Stable document content.",
                &source.id,
                store_id,
            )),
            ScriptStep::Resource(make_resource(
                changing_uri,
                "Completely new content.",
                &source.id,
                store_id,
            )),
        ]);
        let deps2 = SourceIngestionDeps {
            doc_index: &mut doc_index2,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result2 = run_source_ingestion(&source, &ingestor2, deps2)
            .await
            .unwrap();

        assert_eq!(
            result2.docs_indexed, 1,
            "only the changed doc should be re-indexed after rehydration"
        );
        assert_eq!(result2.docs_skipped, 1, "stable doc should be skipped");
    }

    // -----------------------------------------------------------------
    // 5/6. Delete-sweep: not-yielded URI is deleted; yielded URI is kept
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn delete_sweep_removes_uri_not_yielded_keeps_yielded() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");

        let kept_uri = "file:///docs/kept.md";
        let gone_uri = "file:///docs/gone.md";
        let kept_record = seed_indexed(
            &store,
            &embedder,
            &config,
            &source,
            kept_uri,
            "Kept content.",
        )
        .await;
        let gone_record = seed_indexed(
            &store,
            &embedder,
            &config,
            &source,
            gone_uri,
            "Gone content.",
        )
        .await;

        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(kept_record.clone());
        doc_index.upsert(gone_record.clone());

        // This run only yields `kept_uri` — `gone_uri` is simply absent,
        // exactly like a deleted file or a 404'd URL.
        let kept_resource = make_resource(kept_uri, "Kept content.", &source.id, store_id);
        let ingestor = FakeIngestor::new(vec![ScriptStep::Resource(kept_resource)]);

        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(result.docs_deleted, 1);
        let gone_chunks = store
            .get_chunks_for_resource(&gone_record.resource_id)
            .await
            .unwrap();
        assert!(
            gone_chunks.is_empty(),
            "swept resource's chunks must be gone"
        );
        let kept_chunks = store
            .get_chunks_for_resource(&kept_record.resource_id)
            .await
            .unwrap();
        assert!(
            !kept_chunks.is_empty(),
            "yielded resource must survive the sweep"
        );
        assert!(doc_index.get(gone_uri).is_none());
        assert!(doc_index.get(kept_uri).is_some());
    }

    // -----------------------------------------------------------------
    // #185 / #156: "I observed nothing" is not "it was deleted".
    //
    // Three levels of the same conflation, guarded independently:
    //   - the sink   — a zero-chunk resource neither writes nor deletes;
    //   - guard 1    — an incomplete enumeration suppresses the sweep;
    //   - guard 2    — a run that saw none of the source's own URIs
    //                  suppresses the sweep whatever the ingestor claims.
    // -----------------------------------------------------------------

    /// #185 end-to-end: a zero-block `Resource` reaching `on_resource`
    /// must be reported as a skip, must not delete the URI's indexed
    /// content, and — the subtle part — must leave `doc_index` pointing
    /// at the OLD resource. Upserting the empty resource's id/hash while
    /// the store still holds the old resource's rows would leave the
    /// index referencing a resource_id with no rows behind it.
    #[tokio::test]
    async fn zero_block_resource_leaves_doc_index_pointing_at_old_resource() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");

        let uri = "file:///docs/emptied.md";
        let old_record =
            seed_indexed(&store, &embedder, &config, &source, uri, "Original body.").await;

        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(old_record.clone());

        // The file is still there and still enumerated — it just extracted
        // to nothing this run.
        let empty_resource = make_resource_with_blocks(uri, &source.id, store_id, vec![]);
        assert_ne!(
            empty_resource.id, old_record.resource_id,
            "sanity: the empty resource must have its own id, or this test \
             could not distinguish 'index updated' from 'index left alone'"
        );
        let ingestor = FakeIngestor::new(vec![ScriptStep::Resource(empty_resource)]);

        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(
            result.docs_deleted, 0,
            "an empty extraction deletes nothing"
        );
        assert_eq!(
            result.docs_indexed, 0,
            "nothing was written, so nothing was indexed"
        );
        assert_eq!(result.docs_skipped, 1, "the empty resource is a skip");
        assert_eq!(result.error_count, 0, "an empty extraction is not an error");

        let old_chunks = store
            .get_chunks_for_resource(&old_record.resource_id)
            .await
            .unwrap();
        assert!(
            !old_chunks.is_empty(),
            "the previously indexed content must still be searchable"
        );

        let record = doc_index.get(uri).expect("the URI must survive the sweep");
        assert_eq!(
            record.resource_id, old_record.resource_id,
            "doc_index must still point at the resource whose rows the \
             store actually holds"
        );
        assert_eq!(record.content_hash, old_record.content_hash);
    }

    /// Guard 1 (#156): an ingestor that reports `Enumeration::Incomplete`
    /// has told us it could not see the source. Its zero observations are
    /// no evidence of deletion, so the sweep must not run.
    #[tokio::test]
    async fn unavailable_enumeration_skips_sweep() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");

        let uri = "file:///volumes/archive/book.md";
        let record = seed_indexed(&store, &embedder, &config, &source, uri, "Book text.").await;

        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(record.clone());

        // Zero callbacks of any kind — exactly what `FileIngestor` does
        // when its root is an unmounted volume.
        let ingestor = FakeIngestor::incomplete("source root is not reachable: /volumes/archive");

        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(
            result.docs_deleted, 0,
            "an unreachable source must not delete its documents — this is \
             the #156 incident in miniature"
        );
        let chunks = store
            .get_chunks_for_resource(&record.resource_id)
            .await
            .unwrap();
        assert!(
            !chunks.is_empty(),
            "chunks must survive an unreachable root"
        );
        assert!(
            doc_index.get(uri).is_some(),
            "the doc_index record must survive too, or the next successful \
             run would re-index everything from scratch"
        );
    }

    /// Guard 2 (#156): source-shape-agnostic backstop. Even when the
    /// ingestor claims a *complete* enumeration, a run that observed none
    /// of the URIs this source owns is far more likely to be a broken
    /// connector than a source whose entire contents vanished at once.
    #[tokio::test]
    async fn zero_seen_run_does_not_sweep_source_with_history() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");

        let a = seed_indexed(
            &store,
            &embedder,
            &config,
            &source,
            "file:///docs/a.md",
            "Alpha.",
        )
        .await;
        let b = seed_indexed(
            &store,
            &embedder,
            &config,
            &source,
            "file:///docs/b.md",
            "Bravo.",
        )
        .await;

        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(a.clone());
        doc_index.upsert(b.clone());

        // A well-behaved-looking run that nevertheless yielded nothing.
        let ingestor = FakeIngestor::new(vec![ScriptStep::Discovered(0)]);

        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(
            result.docs_deleted,
            0,
            "a run that saw none of the source's {} known URIs must not \
             sweep them",
            doc_index.len()
        );
        for record in [&a, &b] {
            let chunks = store
                .get_chunks_for_resource(&record.resource_id)
                .await
                .unwrap();
            assert!(!chunks.is_empty(), "chunks for {} must survive", record.uri);
        }
    }

    /// This behavior must stay exactly as it is: for a path/url source,
    /// the zero-seen backstop is the same shape of "this run should have
    /// produced full evidence and didn't" as guard 1, so it warns
    /// unconditionally, regardless of the feed branch's own move to
    /// `debug!` for the same guard (feed's routine steady state is a
    /// 304, which has no equivalent here).
    #[tokio::test]
    async fn zero_seen_suppression_on_a_path_source_still_logs_at_warn() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");

        let a = seed_indexed(
            &store,
            &embedder,
            &config,
            &source,
            "file:///docs/a.md",
            "Alpha.",
        )
        .await;
        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(a);

        let ingestor = FakeIngestor::new(vec![ScriptStep::Discovered(0)]);
        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };

        let (result, captured) = run_capturing_logs(&source, &ingestor, deps).await;
        result.unwrap();
        assert!(
            captured.contains("WARN") && captured.contains("skipping delete-sweep"),
            "the path/url zero-seen backstop must still log at WARN, unchanged by \
             the feed branch's move to debug for the same guard; captured: {captured}"
        );
    }

    /// Guard 2 must not over-suppress: seeing *any* owned URI licenses the
    /// sweep for the rest. (`delete_sweep_removes_uri_not_yielded_keeps_yielded`
    /// covers the same shape; this states the guard's boundary directly,
    /// with a source that owns several URIs and reports only one.)
    #[tokio::test]
    async fn sweep_still_runs_when_any_owned_uri_is_seen() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");

        let kept_uri = "file:///docs/kept.md";
        let gone_a = "file:///docs/gone-a.md";
        let gone_b = "file:///docs/gone-b.md";

        let kept = seed_indexed(&store, &embedder, &config, &source, kept_uri, "Kept.").await;
        let a = seed_indexed(&store, &embedder, &config, &source, gone_a, "Gone A.").await;
        let b = seed_indexed(&store, &embedder, &config, &source, gone_b, "Gone B.").await;

        let mut doc_index = DocumentIndex::new();
        for record in [&kept, &a, &b] {
            doc_index.upsert(record.clone());
        }

        // One of three URIs observed — the other two really were deleted.
        let kept_resource = make_resource(kept_uri, "Kept.", &source.id, store_id);
        let ingestor = FakeIngestor::new(vec![ScriptStep::Resource(kept_resource)]);

        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(
            result.docs_deleted, 2,
            "legitimate deletion must still work — the guards suppress the \
             sweep only when the run observed nothing at all"
        );
        assert!(doc_index.get(gone_a).is_none());
        assert!(doc_index.get(gone_b).is_none());
        assert!(doc_index.get(kept_uri).is_some());
    }

    // -----------------------------------------------------------------
    // DeletionPolicy::Retain — the default. Nothing is ever removed
    // unless the operator passes `--delete` (rsync semantics).
    // -----------------------------------------------------------------

    /// The default policy removes nothing and reports what `--delete`
    /// would have removed. This is the same fixture as
    /// `delete_sweep_removes_uri_not_yielded_keeps_yielded`, differing
    /// only in the policy — so the two together isolate the flag's effect.
    #[tokio::test]
    async fn retain_policy_keeps_absent_documents_and_counts_them_prunable() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");

        let kept_uri = "file:///docs/kept.md";
        let gone_uri = "file:///docs/gone.md";
        let kept = seed_indexed(&store, &embedder, &config, &source, kept_uri, "Kept.").await;
        let gone = seed_indexed(&store, &embedder, &config, &source, gone_uri, "Gone.").await;

        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(kept.clone());
        doc_index.upsert(gone.clone());

        let ingestor = FakeIngestor::new(vec![ScriptStep::Resource(make_resource(
            kept_uri, "Kept.", &source.id, store_id,
        ))]);

        let deps = SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config);
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(
            result.docs_deleted, 0,
            "the default policy must never delete"
        );
        assert_eq!(
            result.docs_prunable, 1,
            "the absent document must be reported as prunable so the CLI \
             can tell the user what --delete would remove"
        );
        let chunks = store
            .get_chunks_for_resource(&gone.resource_id)
            .await
            .unwrap();
        assert!(!chunks.is_empty(), "retained document's chunks stay");
        assert!(
            doc_index.get(gone_uri).is_some(),
            "a retained document must stay in the index too, or the next \
             run would re-index it as new"
        );
    }

    /// Retention covers positively-confirmed deletions as well. An
    /// archived copy of a page that has since 404'd is often the most
    /// valuable thing in the index — "the origin dropped it" is not "you
    /// wanted it dropped."
    #[tokio::test]
    async fn retain_policy_keeps_confirmed_gone_documents() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = Source {
            id: new_ulid(),
            store_id: store_id.to_string(),
            kind: SourceKind::Url,
            spec: SourceSpec::Url {
                url: "https://example.com/article".to_string(),
                refresh_interval_secs: None,
            },
            source_preset: "prose".to_string(),
        };

        let url = "https://example.com/article";
        let record = seed_indexed(&store, &embedder, &config, &source, url, "Article body.").await;
        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(record.clone());

        let ingestor = FakeIngestor::new(vec![ScriptStep::Gone(url.to_string())]);
        let deps = SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config);
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(result.docs_deleted, 0);
        assert_eq!(result.docs_prunable, 1);
        let chunks = store
            .get_chunks_for_resource(&record.resource_id)
            .await
            .unwrap();
        assert!(
            !chunks.is_empty(),
            "a 404'd article stays searchable by default"
        );
    }

    /// A guard-suppressed sweep must NOT inflate `docs_prunable`: those
    /// documents would not be removed even under `--delete`, so telling
    /// the user "N could be pruned" would be a lie that invites them to
    /// pass the flag expecting a cleanup that cannot happen.
    #[tokio::test]
    async fn suppressed_sweep_reports_nothing_prunable() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");

        let record = seed_indexed(
            &store,
            &embedder,
            &config,
            &source,
            "file:///volumes/archive/a.md",
            "Body.",
        )
        .await;
        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(record.clone());

        let ingestor = FakeIngestor::incomplete("source root is not reachable: /volumes/archive");
        let deps = SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config);
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(result.docs_deleted, 0);
        assert_eq!(
            result.docs_prunable, 0,
            "an unreachable root makes nothing prunable — --delete would \
             not remove these either"
        );
    }

    /// Guard 2 must not fire for a source with no history: a brand-new
    /// source that legitimately enumerates zero documents has nothing to
    /// preserve, and suppressing its (no-op) sweep would be meaningless.
    /// Stated as a test so the "N > 0" half of the condition can't be
    /// dropped silently.
    #[tokio::test]
    async fn zero_seen_run_on_source_without_history_is_harmless() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");
        let other = make_source_with_preset(store_id, "prose");

        // A sibling source's document — this source owns nothing.
        let foreign = seed_indexed(
            &store,
            &embedder,
            &config,
            &other,
            "file:///other/x.md",
            "Foreign.",
        )
        .await;
        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(foreign.clone());

        let ingestor = FakeIngestor::new(vec![ScriptStep::Discovered(0)]);
        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(result.docs_deleted, 0);
        let chunks = store
            .get_chunks_for_resource(&foreign.resource_id)
            .await
            .unwrap();
        assert!(
            !chunks.is_empty(),
            "another source's document is never this source's to sweep"
        );
    }

    // -----------------------------------------------------------------
    // 5b. Regression: delete-sweep must fire for a file under a
    // space-containing root. Before the sweep filtered by `source_id`,
    // it matched URIs against a prefix built from the raw
    // (non-percent-encoded) canonical root, which never matched the
    // percent-encoded `Resource.uri` a real file ingestor produces —
    // so a deleted file under such a root was silently never swept
    // (stale chunks live forever).
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn delete_sweep_removes_file_under_space_containing_root() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("My Docs")).unwrap();
        std::fs::write(
            dir.path().join("My Docs").join("note.md"),
            b"Space root content.",
        )
        .unwrap();
        // A second file that survives this run. Without it the source
        // would own exactly one URI and observe none of them, tripping
        // the #156 zero-seen guard — which would mask what this test is
        // actually about (URI encoding in the sweep's ownership check).
        std::fs::write(
            dir.path().join("My Docs").join("keep.md"),
            b"Still here content.",
        )
        .unwrap();
        let root = dir.path().join("My Docs").canonicalize().unwrap();

        let source = Source {
            id: new_ulid(),
            store_id: store_id.to_string(),
            kind: SourceKind::Path,
            spec: SourceSpec::Path {
                root: root.to_str().unwrap().to_string(),
                include: vec![],
                exclude: vec![],
            },
            source_preset: "prose".to_string(),
        };

        // Enumerate for real — this is exactly how the URI the doc_index
        // stores is shaped in production (`FoundFile.uri` is already a
        // normalized `Uri`, built via `Uri::from_file_path`).
        let found = enumerate_path_source(root.to_str().unwrap(), &[], &[])
            .unwrap()
            .files()
            .to_vec();
        assert_eq!(found.len(), 2);
        let uri_of = |name: &str| {
            found
                .iter()
                .find(|f| f.path.ends_with(name))
                .unwrap_or_else(|| panic!("{name} must be enumerated"))
                .uri
                .clone()
        };
        let normalized_uri = uri_of("note.md");
        let kept_uri = uri_of("keep.md");
        assert!(
            normalized_uri.as_str().contains("My%20Docs"),
            "sanity: the space must be percent-encoded in the indexed URI"
        );

        let record = seed_indexed(
            &store,
            &embedder,
            &config,
            &source,
            normalized_uri.as_str(),
            "Space root content.",
        )
        .await;
        let kept_record = seed_indexed(
            &store,
            &embedder,
            &config,
            &source,
            kept_uri.as_str(),
            "Still here content.",
        )
        .await;

        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(record.clone());
        doc_index.upsert(kept_record.clone());

        // Simulate `note.md` having been deleted from disk: this run
        // yields only `keep.md`.
        let ingestor = FakeIngestor::new(vec![ScriptStep::Resource(make_resource(
            kept_uri.as_str(),
            "Still here content.",
            &source.id,
            store_id,
        ))]);
        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(
            result.docs_deleted, 1,
            "the file under the space-containing root must be swept"
        );
        let chunks = store
            .get_chunks_for_resource(&record.resource_id)
            .await
            .unwrap();
        assert!(chunks.is_empty(), "swept resource's chunks must be gone");
        assert!(doc_index.get(normalized_uri.as_str()).is_none());
        assert!(
            doc_index.get(kept_uri.as_str()).is_some(),
            "the still-present file under the same root must survive"
        );
    }

    /// Same shape as the space-root sweep above, but with a reserved URI
    /// delimiter in the root. `Uri::from_file_path` encodes `#` as `%23`,
    /// while URI-shape heuristics built on `Uri::parse` truncate at `#`
    /// (it opens a fragment) — historically that divergence made the
    /// sweep silently skip such records, leaving the deleted file's
    /// chunks searchable forever. Ownership by `source_id` is immune to
    /// the root's encoding; this pins that.
    #[cfg(unix)]
    #[tokio::test]
    async fn delete_sweep_removes_file_under_hash_containing_root() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("my#notes")).unwrap();
        std::fs::write(
            dir.path().join("my#notes").join("note.md"),
            b"Hash root content.",
        )
        .unwrap();
        // Second file survives this run — see the space-root test above
        // for why a lone owned URI would trip the #156 zero-seen guard
        // and mask what this test is pinning.
        std::fs::write(
            dir.path().join("my#notes").join("keep.md"),
            b"Still here content.",
        )
        .unwrap();
        let root = dir.path().join("my#notes").canonicalize().unwrap();

        let source = Source {
            id: new_ulid(),
            store_id: store_id.to_string(),
            kind: SourceKind::Path,
            spec: SourceSpec::Path {
                root: root.to_str().unwrap().to_string(),
                include: vec![],
                exclude: vec![],
            },
            source_preset: "prose".to_string(),
        };

        let found = enumerate_path_source(root.to_str().unwrap(), &[], &[])
            .unwrap()
            .files()
            .to_vec();
        assert_eq!(found.len(), 2);
        let uri_of = |name: &str| {
            found
                .iter()
                .find(|f| f.path.ends_with(name))
                .unwrap_or_else(|| panic!("{name} must be enumerated"))
                .uri
                .clone()
        };
        let normalized_uri = uri_of("note.md");
        let kept_uri = uri_of("keep.md");
        assert!(
            normalized_uri.as_str().contains("my%23notes"),
            "sanity: the `#` must be percent-encoded in the indexed URI"
        );

        let record = seed_indexed(
            &store,
            &embedder,
            &config,
            &source,
            normalized_uri.as_str(),
            "Hash root content.",
        )
        .await;
        let kept_record = seed_indexed(
            &store,
            &embedder,
            &config,
            &source,
            kept_uri.as_str(),
            "Still here content.",
        )
        .await;

        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(record.clone());
        doc_index.upsert(kept_record.clone());

        // `note.md` is gone from disk: this run yields only `keep.md`.
        let ingestor = FakeIngestor::new(vec![ScriptStep::Resource(make_resource(
            kept_uri.as_str(),
            "Still here content.",
            &source.id,
            store_id,
        ))]);
        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(
            result.docs_deleted, 1,
            "the file under the `#`-containing root must be swept"
        );
        let chunks = store
            .get_chunks_for_resource(&record.resource_id)
            .await
            .unwrap();
        assert!(chunks.is_empty(), "swept resource's chunks must be gone");
        assert!(doc_index.get(normalized_uri.as_str()).is_none());
    }

    // -----------------------------------------------------------------
    // 6b. C0 regression: delete-sweep boundary safety across sibling
    //     path sources whose roots are string prefixes of each other
    //     (e.g. /data/blog vs /data/blog-drafts). Sweeping source A must
    //     never delete source B's live resources just because B's root
    //     string happens to start with A's root string.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn delete_sweep_does_not_cross_sibling_prefix_sources() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);

        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("blog")).unwrap();
        std::fs::create_dir_all(base.path().join("blog-drafts")).unwrap();
        let blog_root = base.path().join("blog").canonicalize().unwrap();
        let blog_drafts_root = base.path().join("blog-drafts").canonicalize().unwrap();

        let source_a = Source {
            id: new_ulid(),
            store_id: store_id.to_string(),
            kind: SourceKind::Path,
            spec: SourceSpec::Path {
                root: blog_root.to_str().unwrap().to_string(),
                include: vec![],
                exclude: vec![],
            },
            source_preset: "prose".to_string(),
        };
        let source_b = Source {
            id: new_ulid(),
            store_id: store_id.to_string(),
            kind: SourceKind::Path,
            spec: SourceSpec::Path {
                root: blog_drafts_root.to_str().unwrap().to_string(),
                include: vec![],
                exclude: vec![],
            },
            source_preset: "prose".to_string(),
        };

        let uri_a = format!("file://{}/post.md", blog_root.display());
        let uri_a_kept = format!("file://{}/kept.md", blog_root.display());
        let uri_b = format!("file://{}/draft.md", blog_drafts_root.display());

        // Both sources' documents share the same store-level doc_index —
        // exactly the shared-store scenario the finding describes.
        let record_a =
            seed_indexed(&store, &embedder, &config, &source_a, &uri_a, "Blog post.").await;
        // A second document under source A that survives this run. Source
        // A must observe at least one of its own URIs or the #156
        // zero-seen guard suppresses its sweep entirely, which would make
        // this test vacuous rather than failing loudly.
        let record_a_kept = seed_indexed(
            &store,
            &embedder,
            &config,
            &source_a,
            &uri_a_kept,
            "Kept post.",
        )
        .await;
        let record_b = seed_indexed(
            &store,
            &embedder,
            &config,
            &source_b,
            &uri_b,
            "Draft content.",
        )
        .await;

        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(record_a.clone());
        doc_index.upsert(record_a_kept.clone());
        doc_index.upsert(record_b.clone());

        // Sweep source A only: `post.md` is gone from disk, `kept.md`
        // still there. Source B's ingestor does NOT run this cycle.
        let ingestor = FakeIngestor::new(vec![ScriptStep::Resource(make_resource(
            &uri_a_kept,
            "Kept post.",
            &source_a.id,
            store_id,
        ))]);
        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result = run_source_ingestion(&source_a, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(
            result.docs_deleted, 1,
            "only source A's own (now-absent) document is swept"
        );
        let a_chunks = store
            .get_chunks_for_resource(&record_a.resource_id)
            .await
            .unwrap();
        assert!(a_chunks.is_empty(), "source A's document must be deleted");

        let b_chunks = store
            .get_chunks_for_resource(&record_b.resource_id)
            .await
            .unwrap();
        assert!(
            !b_chunks.is_empty(),
            "source B's document must survive sweeping source A, even though \
             B's root string starts with A's root string"
        );
        assert!(
            doc_index.get(&record_b.uri).is_some(),
            "source B's doc_index record must remain"
        );
    }

    /// Percent-encoding twin roots: source A's root is the *literal*
    /// directory name `foo%23`, source B's root is `foo#`. B's documents
    /// are stored under `file://…/foo%23/…` (canonical
    /// `Uri::from_file_path` encodes `#` as `%23`) — byte-identical to
    /// what a `Uri::parse`-built prefix for A's root produces, since
    /// `%23` is already a valid percent-encoding that `Url::parse`
    /// preserves. Any string-prefix heuristic therefore attributes B's
    /// live rows to A, and sweeping only source A deletes B's documents.
    /// The sweep must decide ownership by `source_id`, not by URI shape.
    #[cfg(unix)]
    #[tokio::test]
    async fn delete_sweep_does_not_cross_percent_encoded_twin_roots() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);

        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("foo%23")).unwrap();
        std::fs::create_dir_all(base.path().join("foo#")).unwrap();
        std::fs::write(
            base.path().join("foo#").join("doc.md"),
            b"Twin root content.",
        )
        .unwrap();
        let root_a = base.path().join("foo%23").canonicalize().unwrap();
        let root_b = base.path().join("foo#").canonicalize().unwrap();

        let source_a = Source {
            id: new_ulid(),
            store_id: store_id.to_string(),
            kind: SourceKind::Path,
            spec: SourceSpec::Path {
                root: root_a.to_str().unwrap().to_string(),
                include: vec![],
                exclude: vec![],
            },
            source_preset: "prose".to_string(),
        };
        let source_b = Source {
            id: new_ulid(),
            store_id: store_id.to_string(),
            kind: SourceKind::Path,
            spec: SourceSpec::Path {
                root: root_b.to_str().unwrap().to_string(),
                include: vec![],
                exclude: vec![],
            },
            source_preset: "prose".to_string(),
        };

        // Enumerate B's root for real, so the stored URI is shaped exactly
        // as production shapes it.
        let found = enumerate_path_source(root_b.to_str().unwrap(), &[], &[])
            .unwrap()
            .files()
            .to_vec();
        assert_eq!(found.len(), 1);
        let uri_b = found[0].uri.as_str().to_string();
        assert!(
            uri_b.contains("foo%23/"),
            "sanity: B's canonical URI must encode `#` as `%23`, making it \
             collide with A's literal `foo%23` root"
        );

        let record_b = seed_indexed(
            &store,
            &embedder,
            &config,
            &source_b,
            &uri_b,
            "Twin root content.",
        )
        .await;

        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(record_b.clone());

        // Sweep source A only (its directory is empty; B does not run
        // this cycle — e.g. `index --source A`).
        let ingestor = FakeIngestor::new(vec![]);
        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result = run_source_ingestion(&source_a, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(
            result.docs_deleted, 0,
            "sweeping source A must not delete source B's live document, \
             even though A's literal `foo%23` root and B's encoded `foo#` \
             root produce byte-identical URI prefixes"
        );
        let b_chunks = store
            .get_chunks_for_resource(&record_b.resource_id)
            .await
            .unwrap();
        assert!(
            !b_chunks.is_empty(),
            "source B's chunks must survive sweeping source A"
        );
        assert!(
            doc_index.get(&record_b.uri).is_some(),
            "source B's doc_index record must remain"
        );
    }

    // -----------------------------------------------------------------
    // 7. on_skipped(Unchanged) marks the URI seen — survives the sweep
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn on_skipped_unchanged_survives_delete_sweep() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");

        let uri = "file:///docs/prefiltered.md";
        let record = seed_indexed(&store, &embedder, &config, &source, uri, "Content.").await;

        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(record.clone());

        // The ingestor pre-filters this URI itself (e.g. mtime unchanged)
        // and never calls on_resource for it at all.
        let ingestor = FakeIngestor::new(vec![ScriptStep::Skipped(
            uri.to_string(),
            SkipReason::Unchanged,
        )]);

        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(result.docs_deleted, 0);
        assert_eq!(result.docs_skipped, 1);
        let chunks = store
            .get_chunks_for_resource(&record.resource_id)
            .await
            .unwrap();
        assert!(
            !chunks.is_empty(),
            "on_skipped(Unchanged) must not delete existing chunks"
        );
        assert!(doc_index.get(uri).is_some());
    }

    // -----------------------------------------------------------------
    // 8. A confirmed-Gone URL is deleted (Url-kind source).
    //
    // Renamed from `gone_url_style_absence_is_swept`: since #156 the
    // deletion no longer rides on *absence* — the ingestor reports the
    // 404/410 positively via `on_gone`, and that path is exempt from the
    // sweep guards precisely because nothing about it is inferred.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn confirmed_gone_url_is_deleted() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = Source {
            id: new_ulid(),
            store_id: store_id.to_string(),
            kind: SourceKind::Url,
            spec: SourceSpec::Url {
                url: "https://example.com/page".to_string(),
                refresh_interval_secs: None,
            },
            source_preset: "prose".to_string(),
        };

        let url = "https://example.com/page";
        let record = seed_indexed(&store, &embedder, &config, &source, url, "Page body.").await;

        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(record.clone());

        // The URL now 404s/410s. `UrlIngestor` reports that positively via
        // `on_gone` rather than by staying silent: since #156 an absence
        // alone no longer licenses a delete, but a confirmed 410 is
        // knowledge — the origin answered — so it deletes regardless of
        // the sweep guards.
        let ingestor = FakeIngestor::new(vec![ScriptStep::Gone(url.to_string())]);

        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(result.docs_deleted, 1);
        let chunks = store
            .get_chunks_for_resource(&record.resource_id)
            .await
            .unwrap();
        assert!(chunks.is_empty());
    }

    // -----------------------------------------------------------------
    // 8f. C1: feed sources are exempt from the delete-sweep. A feed only
    // ever exposes its most-recent N entries, so a zero-callback run
    // (absent entries scrolled off the window, or a feed-level 304 Not
    // Modified) must NOT delete previously-indexed entries — while a url
    // source that positively confirms its URL is Gone must still delete
    // it. Test 8 above covers the url half alone; this test additionally
    // proves the two behaviors coexist correctly in the same
    // store/doc_index.
    //
    // Note what changed with #156: the two scenarios are no longer
    // "identically-shaped zero-callback runs" distinguished only by
    // source kind. The url source now *says* the URL is gone. Silence
    // means the same thing for both kinds now — no evidence — which is
    // why the feed exemption and the sweep guards can coexist without
    // one having to special-case the other.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn feed_zero_callback_run_is_not_swept_but_confirmed_gone_url_is_deleted() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);

        let feed_source = Source {
            id: new_ulid(),
            store_id: store_id.to_string(),
            kind: SourceKind::Feed,
            spec: SourceSpec::Feed {
                url: "https://example.com/feed.xml".to_string(),
                max_entries: None,
                fetch_full_content: true,
                refresh_interval_secs: None,
            },
            source_preset: "prose".to_string(),
        };
        let url_source = Source {
            id: new_ulid(),
            store_id: store_id.to_string(),
            kind: SourceKind::Url,
            spec: SourceSpec::Url {
                url: "https://example.com/page".to_string(),
                refresh_interval_secs: None,
            },
            source_preset: "prose".to_string(),
        };

        let feed_entry_uri = "https://example.com/feed.xml#entry:1";
        let url_uri = "https://example.com/page";

        let feed_record = seed_indexed(
            &store,
            &embedder,
            &config,
            &feed_source,
            feed_entry_uri,
            "Feed entry body.",
        )
        .await;
        let url_record = seed_indexed(
            &store,
            &embedder,
            &config,
            &url_source,
            url_uri,
            "Page body.",
        )
        .await;

        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(feed_record.clone());
        doc_index.upsert(url_record.clone());

        // The feed's ingestor yields nothing at all — a feed-level 304 Not
        // Modified, or the entry simply having scrolled off the feed's
        // window. Silence, carrying no information.
        let feed_ingestor = FakeIngestor::new(vec![]);
        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let feed_result = run_source_ingestion(&feed_source, &feed_ingestor, deps)
            .await
            .unwrap();

        assert_eq!(
            feed_result.docs_deleted, 0,
            "feed sources are exempt from the delete-sweep — a zero-callback \
             run must not delete"
        );
        let feed_chunks = store
            .get_chunks_for_resource(&feed_record.resource_id)
            .await
            .unwrap();
        assert!(
            !feed_chunks.is_empty(),
            "feed entry's chunks must survive an unswept run"
        );
        assert!(doc_index.get(feed_entry_uri).is_some());

        // The url source's fetch came back 404/410 — knowledge, reported
        // positively.
        let url_ingestor = FakeIngestor::new(vec![ScriptStep::Gone(url_uri.to_string())]);
        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let url_result = run_source_ingestion(&url_source, &url_ingestor, deps)
            .await
            .unwrap();

        assert_eq!(
            url_result.docs_deleted, 1,
            "a confirmed-Gone URL in the very same store/doc_index is still \
             deleted — the feed exemption is about absence, not about \
             refusing to act on knowledge"
        );
        let url_chunks = store
            .get_chunks_for_resource(&url_record.resource_id)
            .await
            .unwrap();
        assert!(
            url_chunks.is_empty(),
            "swept url resource's chunks must be gone"
        );
    }

    #[tokio::test]
    async fn source_location_feed_arm_returns_url() {
        let source = Source {
            id: new_ulid(),
            store_id: "store-1".to_string(),
            kind: SourceKind::Feed,
            spec: SourceSpec::Feed {
                url: "https://example.com/feed.xml".to_string(),
                max_entries: None,
                fetch_full_content: true,
                refresh_interval_secs: None,
            },
            source_preset: "prose".to_string(),
        };
        assert_eq!(source_location(&source), "https://example.com/feed.xml");
    }

    // -----------------------------------------------------------------
    // 8b-8e (removed by the `on_skipped(&Uri, ...)` signature change):
    // these four tests fed a RAW locator string through
    // `ScriptStep::Skipped` to prove `PipelineCallback::on_skipped`
    // normalized it before using it for `seen`/progress bookkeeping.
    // Once `Ingestor::on_skipped` takes `&Uri` instead of `&str`, there
    // is no longer any way to construct that raw input at all —
    // `FakeIngestor` itself must call `Uri::parse` on the script's
    // string before handing it to `on_skipped`, so any space/casing
    // divergence is already gone by the time production code sees it.
    // The tests would still pass with the normalization call deleted
    // from `on_skipped` entirely (which this commit does): there is no
    // longer a single-line revert of production code that makes any of
    // them fail, which makes them tautological guards, not regression
    // tests. They are deleted rather than kept as dead weight.
    //
    // The unparseable-locator fallback test is replaced by
    // `ingest::url_ingestor`'s `invalid_config_url_fails_fast`, which
    // tests the only place that class of input can still occur: a raw,
    // never-validated config string, now rejected eagerly by the
    // hoisted `Uri::parse` at the top of `UrlIngestor::ingest`.
    //
    // The durable, non-tautological regression coverage for the
    // original bug lives in
    // `ingest/tests/file_ingestor_sweep_regression.rs`, which drives the
    // real `FileIngestor` over a real space-named file end to end and
    // does not go through `FakeIngestor` at all.

    // -----------------------------------------------------------------
    // 9. A per-resource error doesn't abort the run — later resources
    //    still index
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn per_resource_error_does_not_abort_later_resources_still_index() {
        let store = FakeStore::new();
        let embedder = SelectiveFailEmbedder {
            fail_marker: "FAIL_MARKER",
            inner: FakeEmbedder::new(4),
        };
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");

        let first = make_resource(
            "file:///docs/first.md",
            "First good content.",
            &source.id,
            store_id,
        );
        let bad = make_resource(
            "file:///docs/bad.md",
            "This has FAIL_MARKER in it.",
            &source.id,
            store_id,
        );
        let last = make_resource(
            "file:///docs/last.md",
            "Last good content.",
            &source.id,
            store_id,
        );
        let first_id = first.id.clone();
        let last_id = last.id.clone();

        let ingestor = FakeIngestor::new(vec![
            ScriptStep::Resource(first),
            ScriptStep::Resource(bad),
            ScriptStep::Resource(last),
        ]);

        let mut doc_index = DocumentIndex::new();
        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(result.error_count, 1);
        assert_eq!(result.docs_indexed, 2, "the two good resources both index");
        assert!(!store
            .get_chunks_for_resource(&first_id)
            .await
            .unwrap()
            .is_empty());
        assert!(!store
            .get_chunks_for_resource(&last_id)
            .await
            .unwrap()
            .is_empty());
    }

    // -----------------------------------------------------------------
    // 10. Embed-failure ⇒ error counted, no delete of existing chunks
    //     (crash-safety, A6)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn embed_failure_preserves_existing_chunks_and_counts_error() {
        let store = FakeStore::new();
        let good_embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");

        let uri = "file:///docs/doc.md";
        let record = seed_indexed(
            &store,
            &good_embedder,
            &config,
            &source,
            uri,
            "Original content for the document.",
        )
        .await;
        let original_id = record.resource_id.clone();

        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(record);

        let changed = make_resource(
            uri,
            "Changed content that triggers re-indexing.",
            &source.id,
            store_id,
        );
        let ingestor = FakeIngestor::new(vec![ScriptStep::Resource(changed)]);

        let failing_embedder = FailingEmbedder;
        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &failing_embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        };
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .unwrap();

        assert_eq!(result.error_count, 1);
        assert_eq!(result.docs_indexed, 0);
        let chunks = store.get_chunks_for_resource(&original_id).await.unwrap();
        assert!(
            !chunks.is_empty(),
            "a failed re-index must never delete the previously-indexed chunks"
        );
        // doc_index must still point at the old (still-present) resource_id.
        assert_eq!(doc_index.get(uri).unwrap().resource_id, original_id);
    }

    /// F4: a short embedder response (fewer vectors than chunks) returns
    /// an Internal error from `index_resource`.
    #[tokio::test]
    async fn index_resource_short_embedder_returns_error() {
        let store = FakeStore::new();
        let short_embedder = ShortEmbedder { dim: 4 };
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");

        let resource = make_resource(
            "file:///docs/short.md",
            "Content that produces at least one chunk.",
            &source.id,
            store_id,
        );

        let deps = IndexResourceDeps {
            store: &store,
            embedder: &short_embedder,
            config: &config,
        };
        let result = index_resource(&resource, &source, None, &deps).await;

        assert!(
            result.is_err(),
            "must return an error when embedder returns fewer vectors than chunks"
        );
        assert!(
            matches!(result.unwrap_err(), Error::Internal { .. }),
            "error must be Internal"
        );
    }

    // -----------------------------------------------------------------
    // 10b. Replace wiring (issue #79): a single upsert_chunks_and_blocks
    //      call folds the delete in, rather than a separate delete call.
    // -----------------------------------------------------------------

    /// One recorded `upsert_chunks_and_blocks` call: `(store_id, resource_id,
    /// records.len(), replaces_resource_id)`.
    type UpsertCall = (String, String, usize, Option<String>);

    /// Wraps a `FakeStore`, recording every `delete_by_resource` and
    /// `upsert_chunks_and_blocks` call so tests can assert on *how*
    /// `index_resource` drives the store, not just the end state.
    ///
    /// `upsert_chunks_and_blocks` can be told to fail via `fail_next_upsert`;
    /// when it does, it returns an error *without* touching the underlying
    /// `FakeStore` at all (neither delete nor insert), simulating the
    /// all-or-nothing behavior a real atomic transaction provides. This lets
    /// tests verify that `index_resource` itself never performs a separate
    /// delete before calling `upsert_chunks_and_blocks` — if it did, the old
    /// resource would be gone even though the replace as a whole failed.
    struct RecordingStore {
        inner: FakeStore,
        delete_calls: tokio::sync::Mutex<Vec<String>>,
        upsert_calls: tokio::sync::Mutex<Vec<UpsertCall>>,
        fail_next_upsert: std::sync::atomic::AtomicBool,
    }

    impl RecordingStore {
        fn new() -> Self {
            Self {
                inner: FakeStore::new(),
                delete_calls: tokio::sync::Mutex::new(Vec::new()),
                upsert_calls: tokio::sync::Mutex::new(Vec::new()),
                fail_next_upsert: std::sync::atomic::AtomicBool::new(false),
            }
        }

        fn fail_next_upsert(&self) {
            self.fail_next_upsert
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }

        async fn delete_calls(&self) -> Vec<String> {
            self.delete_calls.lock().await.clone()
        }

        async fn upsert_calls(&self) -> Vec<UpsertCall> {
            self.upsert_calls.lock().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl RetrievalStore for RecordingStore {
        async fn upsert_chunks(&self, records: Vec<ChunkRecord>) -> Result<usize, Error> {
            self.inner.upsert_chunks(records).await
        }

        async fn delete_by_resource(&self, resource_id: &str) -> Result<usize, Error> {
            self.delete_calls.lock().await.push(resource_id.to_string());
            self.inner.delete_by_resource(resource_id).await
        }

        async fn delete_by_store(&self, store_id: &str) -> Result<usize, Error> {
            self.inner.delete_by_store(store_id).await
        }

        async fn dense_search(
            &self,
            query_vector: &[f32],
            limit: usize,
            filters: &[crate::store::MetadataFilter],
        ) -> Result<Vec<crate::store::SearchResult>, Error> {
            self.inner.dense_search(query_vector, limit, filters).await
        }

        async fn bm25_search(
            &self,
            query_text: &str,
            limit: usize,
            filters: &[crate::store::MetadataFilter],
        ) -> Result<Vec<crate::store::SearchResult>, Error> {
            self.inner.bm25_search(query_text, limit, filters).await
        }

        async fn stats(&self) -> Result<crate::store::StoreStats, Error> {
            self.inner.stats().await
        }

        async fn get_chunk(&self, chunk_id: &str) -> Result<Option<ChunkRecord>, Error> {
            self.inner.get_chunk(chunk_id).await
        }

        async fn get_chunks_for_resource(
            &self,
            resource_id: &str,
        ) -> Result<Vec<ChunkRecord>, Error> {
            self.inner.get_chunks_for_resource(resource_id).await
        }

        async fn list_indexed_documents(&self) -> Result<Vec<DocumentRecord>, Error> {
            self.inner.list_indexed_documents().await
        }

        async fn update_resource_metadata(
            &self,
            store_id: &str,
            resource_id: &str,
            record: &crate::store::ResourceRecord,
        ) -> Result<(), Error> {
            self.inner
                .update_resource_metadata(store_id, resource_id, record)
                .await
        }

        async fn get_resource_record(
            &self,
            store_id: &str,
            resource_id: &str,
        ) -> Result<Option<crate::store::ResourceRecord>, Error> {
            self.inner.get_resource_record(store_id, resource_id).await
        }

        async fn upsert_chunks_and_blocks(
            &self,
            store_id: &str,
            resource_id: &str,
            records: Vec<ChunkRecord>,
            blocks: &[crate::block::Block],
            replaces_resource_id: Option<&str>,
            _external_last_modified: Option<&str>,
        ) -> Result<usize, Error> {
            self.upsert_calls.lock().await.push((
                store_id.to_string(),
                resource_id.to_string(),
                records.len(),
                replaces_resource_id.map(str::to_string),
            ));

            if self
                .fail_next_upsert
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(Error::Internal {
                    message: "simulated upsert failure".to_string(),
                    correlation_id: "recording_store_simulated_failure".to_string(),
                });
            }

            // Simulate the atomic contract: delete-then-insert, both only
            // observable together since we only reach here when not failing.
            if let Some(old_id) = replaces_resource_id {
                self.inner.delete_by_resource(old_id).await?;
            }
            let count = self.inner.upsert_chunks(records).await?;
            self.inner
                .upsert_blocks(store_id, resource_id, blocks)
                .await?;
            Ok(count)
        }
    }

    #[tokio::test]
    async fn index_resource_replace_uses_single_call_not_separate_delete() {
        let store = RecordingStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");
        let uri = "file:///docs/notes.md";

        let resource_v1 = make_resource(uri, "Version one content.", &source.id, store_id);
        let deps = IndexResourceDeps {
            store: &store,
            embedder: &embedder,
            config: &config,
        };
        index_resource(&resource_v1, &source, None, &deps)
            .await
            .unwrap();
        let old_doc_id = resource_v1.id.clone();

        let resource_v2 = make_resource(
            uri,
            "Version two content - completely different.",
            &source.id,
            store_id,
        );
        index_resource(&resource_v2, &source, Some(&old_doc_id), &deps)
            .await
            .unwrap();

        assert!(
            store.delete_calls().await.is_empty(),
            "index_resource must never call delete_by_resource directly on a \
             content-changed replace — the delete must be folded into the \
             upsert_chunks_and_blocks call"
        );

        let upserts = store.upsert_calls().await;
        assert_eq!(upserts.len(), 2, "one upsert call per index_resource call");
        assert_eq!(
            upserts[0].3, None,
            "first index (no prior document) must not pass replaces_resource_id"
        );
        assert_eq!(
            upserts[1].3,
            Some(old_doc_id),
            "changed-content re-index must pass the old resource_id as \
             replaces_resource_id"
        );
    }

    #[tokio::test]
    async fn index_resource_replace_failure_leaves_old_document_intact() {
        let store = RecordingStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_source_with_preset(store_id, "prose");
        let uri = "file:///docs/notes.md";

        let resource_v1 = make_resource(uri, "Version one content.", &source.id, store_id);
        let deps = IndexResourceDeps {
            store: &store,
            embedder: &embedder,
            config: &config,
        };
        index_resource(&resource_v1, &source, None, &deps)
            .await
            .unwrap();
        let old_doc_id = resource_v1.id.clone();

        let old_chunks_before = store.get_chunks_for_resource(&old_doc_id).await.unwrap();
        assert_eq!(old_chunks_before.len(), 1);

        // Arm the store to fail the *next* upsert_chunks_and_blocks call —
        // i.e. the replace triggered by the content change below.
        store.fail_next_upsert();

        let resource_v2 = make_resource(
            uri,
            "Version two content - completely different.",
            &source.id,
            store_id,
        );
        let result = index_resource(&resource_v2, &source, Some(&old_doc_id), &deps).await;
        assert!(result.is_err(), "the simulated upsert failure must surface");

        // The old document's chunks must still be retrievable — the failed
        // replace must not have removed them via a separate delete call.
        let old_chunks_after = store.get_chunks_for_resource(&old_doc_id).await.unwrap();
        assert_eq!(
            old_chunks_after.len(),
            1,
            "old document chunks must survive a failed replace"
        );
    }

    // -----------------------------------------------------------------
    // 11. window_block_seqs flow through to upserted ChunkRecords for a
    //     messages-preset resource
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn window_block_seqs_flow_through_for_messages_preset() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let source = make_source_with_preset(store_id, "messages");
        let config = IngestionConfig {
            store_id: store_id.to_string(),
            policy_version: "policy-v1".to_string(),
            chunker: ChunkerConfig {
                preset: "messages".to_string(),
                target_tokens: Some(512),
                overlap_tokens: Some(0),
                window_turns: Some(2),
                stride_turns: Some(1),
            },
        };

        let blocks: Vec<Block> = (0..5)
            .map(|i| Block {
                seq: i,
                kind: BlockKind::Message {
                    sender: "alice".to_string(),
                    timestamp: None,
                    message_id: None,
                    reply_to: None,
                },
                text: format!("message number {i}"),
                location: None,
            })
            .collect();

        let resource =
            make_resource_with_blocks("file:///chat/thread.json", &source.id, store_id, blocks);

        let deps = IndexResourceDeps {
            store: &store,
            embedder: &embedder,
            config: &config,
        };
        index_resource(&resource, &source, None, &deps)
            .await
            .unwrap();

        let chunks = store.get_chunks_for_resource(&resource.id).await.unwrap();
        assert!(!chunks.is_empty());
        assert!(
            chunks.iter().any(|c| c.window_block_seqs.len() >= 2),
            "at least one window chunk must span multiple blocks; got: {:?}",
            chunks
                .iter()
                .map(|c| &c.window_block_seqs)
                .collect::<Vec<_>>()
        );
    }

    // -----------------------------------------------------------------
    // 12. Preset gate (#60) — direct unit tests on effective_chunker_config
    // -----------------------------------------------------------------

    #[test]
    fn preset_gate_explicit_code_source_wins_over_md_extension() {
        let base = ChunkerConfig::code();
        let cfg = effective_chunker_config("code", &base, Some("notes.md"), None);
        assert_eq!(cfg.preset, "code");
    }

    #[test]
    fn preset_gate_default_prose_source_auto_routes_rs_file_to_code() {
        let base = ChunkerConfig::prose();
        let cfg = effective_chunker_config("prose", &base, Some("main.rs"), None);
        assert_eq!(cfg.preset, "code");
    }

    #[test]
    fn preset_gate_messages_source_wins_regardless_of_filename() {
        let base = ChunkerConfig::messages();
        let cfg = effective_chunker_config("messages", &base, Some("transcript.md"), None);
        assert_eq!(cfg.preset, "messages");
        assert_eq!(cfg.resolved_window_turns(), 6);
    }

    /// Integration-level check that the preset gate is actually wired into
    /// `index_resource`: an explicit `code` source must not apply the
    /// prose splitter's heading-path attribution to a Markdown file.
    #[tokio::test]
    async fn index_resource_respects_explicit_code_source_preset() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let source = make_source_with_preset(store_id, "code");
        let config = IngestionConfig {
            store_id: store_id.to_string(),
            policy_version: "policy-v1".to_string(),
            chunker: ChunkerConfig::code(),
        };

        let resource = make_resource(
            "file:///docs/notes.md",
            "# Heading\n\nSome prose-looking text under a heading.",
            &source.id,
            store_id,
        );

        let deps = IndexResourceDeps {
            store: &store,
            embedder: &embedder,
            config: &config,
        };
        index_resource(&resource, &source, None, &deps)
            .await
            .unwrap();

        let chunks = store.get_chunks_for_resource(&resource.id).await.unwrap();
        assert!(!chunks.is_empty());
        // The code chunker never derives heading_path (unlike chunk_prose,
        // which would attribute "Heading" here).
        assert!(
            chunks.iter().all(|c| c.heading_path.is_empty()),
            "an explicit code source must route through the code chunker, \
             not the heading-path-aware prose chunker"
        );
    }

    // -----------------------------------------------------------------
    // 13. Title propagation: Resource.title/metadata → ChunkRecord.metadata title
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn title_propagates_from_resource_title_when_metadata_has_none() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let source = make_source_with_preset(store_id, "prose");
        let config = make_ingestion_config(store_id);

        let mut resource = make_resource(
            "file:///docs/titled.md",
            "Body content for the titled document.",
            &source.id,
            store_id,
        );
        resource.title = Some("My Great Title".to_string());
        // metadata's own Dublin Core title is left None (default).

        let deps = IndexResourceDeps {
            store: &store,
            embedder: &embedder,
            config: &config,
        };
        index_resource(&resource, &source, None, &deps)
            .await
            .unwrap();

        let chunks = store.get_chunks_for_resource(&resource.id).await.unwrap();
        assert!(!chunks.is_empty());
        for c in &chunks {
            assert_eq!(c.metadata.title(), Some("My Great Title"));
        }
    }

    #[tokio::test]
    async fn title_from_metadata_is_not_overwritten_by_resource_title() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let source = make_source_with_preset(store_id, "prose");
        let config = make_ingestion_config(store_id);

        let mut resource = make_resource(
            "file:///docs/titled2.md",
            "Body content for the second titled document.",
            &source.id,
            store_id,
        );
        resource.title = Some("Fallback Title".to_string());
        resource.metadata = Metadata::Document(DocumentMetadata {
            dublin_core: DublinCoreMetadata {
                title: Some("Authoritative Title".to_string()),
                ..Default::default()
            },
            ..Default::default()
        });

        let deps = IndexResourceDeps {
            store: &store,
            embedder: &embedder,
            config: &config,
        };
        index_resource(&resource, &source, None, &deps)
            .await
            .unwrap();

        let chunks = store.get_chunks_for_resource(&resource.id).await.unwrap();
        assert!(!chunks.is_empty());
        for c in &chunks {
            assert_eq!(c.metadata.title(), Some("Authoritative Title"));
        }
    }

    // -----------------------------------------------------------------
    // #185: an empty replacement is refused by the sink — it neither
    // writes nor deletes. This test asserted the opposite until #185:
    // "replacing with an empty resource must delete the old chunks" was
    // the documented behavior, and it is exactly how a file that
    // transiently extracts to nothing erased its own indexed content.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn index_resource_empty_blocks_keeps_old_content() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let source = make_source_with_preset(store_id, "prose");
        let config = make_ingestion_config(store_id);

        let old_record = seed_indexed(
            &store,
            &embedder,
            &config,
            &source,
            "file:///docs/e.md",
            "Body.",
        )
        .await;

        let empty_resource =
            make_resource_with_blocks("file:///docs/e.md", &source.id, store_id, vec![]);

        let deps = IndexResourceDeps {
            store: &store,
            embedder: &embedder,
            config: &config,
        };
        let outcome = index_resource(
            &empty_resource,
            &source,
            Some(&old_record.resource_id),
            &deps,
        )
        .await
        .unwrap();

        assert_eq!(outcome, IndexOutcome::Empty);
        let old_chunks = store
            .get_chunks_for_resource(&old_record.resource_id)
            .await
            .unwrap();
        assert!(
            !old_chunks.is_empty(),
            "an empty replacement must not delete the old chunks: the sink \
             cannot tell 'this file is legitimately empty now' apart from \
             'extraction produced nothing this run', and only one of those \
             is evidence the content is gone (#185)"
        );
    }

    /// #103: `index_resource` copies each block's `location.page` onto the
    /// chunk records it writes, keyed by block seq.
    #[tokio::test]
    async fn index_resource_copies_block_page_onto_chunks() {
        use crate::block::{Block, BlockKind, BlockLocation};

        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let source = make_source_with_preset(store_id, "prose");
        let config = make_ingestion_config(store_id);

        let page_block = |seq: u32, text: &str, page: u32| Block {
            seq,
            kind: BlockKind::Text,
            text: text.to_string(),
            location: Some(BlockLocation {
                page: Some(page),
                ..Default::default()
            }),
        };

        let blocks = vec![
            page_block(0, "Alpha content lives on the first page here.", 1),
            page_block(1, "Bravo content lives on the second page here.", 2),
            // A block with no location at all: its chunks must get page None.
            Block {
                seq: 2,
                kind: BlockKind::Text,
                text: "Charlie content has no page info recorded.".to_string(),
                location: None,
            },
        ];

        let resource =
            make_resource_with_blocks("file:///docs/paged.pdf", &source.id, store_id, blocks);
        let deps = IndexResourceDeps {
            store: &store,
            embedder: &embedder,
            config: &config,
        };
        let written = index_resource(&resource, &source, None, &deps)
            .await
            .unwrap();
        assert!(
            matches!(written, IndexOutcome::Written(n, _) if n >= 3),
            "expected at least one chunk per block, got {written:?}"
        );

        let chunks = store.get_chunks_for_resource(&resource.id).await.unwrap();

        // Each chunk's page is that of its originating block seq.
        let page_for_seq = |seq: u32| -> Vec<Option<u32>> {
            chunks
                .iter()
                .filter(|c| c.block_seq == seq)
                .map(|c| c.page)
                .collect()
        };
        assert!(
            page_for_seq(0).iter().all(|p| *p == Some(1)),
            "block 0 → page 1"
        );
        assert!(
            page_for_seq(1).iter().all(|p| *p == Some(2)),
            "block 1 → page 2"
        );
        assert!(
            page_for_seq(2).iter().all(|p| p.is_none()),
            "block 2 has no location → page None"
        );
    }

    // -----------------------------------------------------------------
    // Codex R2: fetched_at is the resource's `added_at` (ingestion time),
    //           never its `modified_at` (a feed-claimed date).
    // -----------------------------------------------------------------

    /// `Provenance.fetched_at` is defined as *acquisition* time, and the
    /// libsql backend binds it to `resources.added_at` — the column
    /// `MetadataFilter::DateAfter`/`DateBefore` (`DateAxis::Added`) filter
    /// on and that every citation reports. `index_resource` used to read
    /// `resource.modified_at`, so a 2020 feed entry ingested today claimed
    /// a 2020 acquisition time and fell outside a "fetched since last
    /// week" filter. Only the feed connector makes the two fields differ
    /// (`file`/`url` set both to the same value), which is why this stayed
    /// latent until the Atom/RSS ingestor landed.
    ///
    /// See specs/02-domain-model.md §4 and its "Timestamps" rule in the
    /// Feed connector section.
    #[tokio::test]
    async fn index_resource_fetched_at_is_added_at_not_modified_at() {
        const INGESTED_AT: &str = "2026-08-05T00:00:00Z";
        const FEED_CLAIMED: &str = "2020-01-01T00:00:00Z";

        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let store_id = "store-1";
        let source = make_source_with_preset(store_id, "prose");
        let config = make_ingestion_config(store_id);

        let mut resource = make_resource(
            "https://blog.example.com/2020/old-post",
            "An old post that a feed is only surfacing to us today.",
            &source.id,
            store_id,
        );
        resource.added_at = INGESTED_AT.to_string();
        resource.modified_at = Some(FEED_CLAIMED.to_string());

        let deps = IndexResourceDeps {
            store: &store,
            embedder: &embedder,
            config: &config,
        };
        index_resource(&resource, &source, None, &deps)
            .await
            .unwrap();

        let chunks = store.get_chunks_for_resource(&resource.id).await.unwrap();
        assert!(!chunks.is_empty(), "the resource must produce chunks");
        for c in &chunks {
            assert_eq!(
                c.fetched_at, INGESTED_AT,
                "fetched_at must be the resource's added_at (ingestion time)"
            );
            assert_ne!(
                c.fetched_at, FEED_CLAIMED,
                "fetched_at must never be the feed-claimed modified_at"
            );
        }
    }

    // -----------------------------------------------------------------
    // 14. lookup_fetch_metadata — the conditional-GET replay seam and
    //     its suppression rule (specs/04-search-pipeline.md §1)
    // -----------------------------------------------------------------

    /// A `PipelineCallback` wired to nothing but what
    /// `lookup_fetch_metadata` itself touches (`doc_index` and
    /// `config.policy_version`) — the store/embedder are never called on
    /// this path, so `FakeStore`/`FakeEmbedder` stand in inertly.
    fn make_pipeline_callback<'a>(
        source: &'a Source,
        doc_index: &'a mut DocumentIndex,
        store: &'a FakeStore,
        embedder: &'a FakeEmbedder,
        config: &'a IngestionConfig,
    ) -> PipelineCallback<'a> {
        PipelineCallback {
            source,
            doc_index,
            store,
            embedder,
            config,
            progress: None,
            result: IngestionResult::default(),
            seen: std::collections::HashSet::new(),
            gone: std::collections::HashSet::new(),
            discovered_total: 0,
            next_index: 0,
            skip_error_count: 0,
        }
    }

    #[tokio::test]
    async fn lookup_fetch_metadata_returns_stored_validators_when_policy_matches() {
        let store_id = "store-1";
        let source = make_source_with_preset(store_id, "prose");
        let config = make_ingestion_config(store_id);
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);

        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(DocumentRecord {
            uri: "https://example.com/doc".to_string(),
            resource_id: "res-1".to_string(),
            source_id: source.id.clone(),
            content_hash: "hash-1".to_string(),
            policy_version: config.policy_version.clone(),
            metadata_hash: "mhash-1".to_string(),
            external_etag: Some("\"abc\"".to_string()),
            external_last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
        });

        let mut callback =
            make_pipeline_callback(&source, &mut doc_index, &store, &embedder, &config);
        let uri = Uri::parse("https://example.com/doc").unwrap();
        let meta = callback.lookup_fetch_metadata(&uri).await;

        assert_eq!(meta.etag.as_deref(), Some("\"abc\""));
        assert_eq!(
            meta.last_modified.as_deref(),
            Some("Wed, 21 Oct 2015 07:28:00 GMT")
        );
    }

    /// The suppression rule, and the one behavior in this seam that must
    /// never regress: a mismatched `policy_version` never replays a
    /// stored validator. A 304 returns no bytes, so a
    /// resource that needs re-chunking under a changed policy could
    /// never be re-chunked if it were allowed to answer 304 — silently
    /// freezing the document at the old policy forever.
    #[tokio::test]
    async fn lookup_fetch_metadata_returns_empty_when_policy_version_differs() {
        let store_id = "store-1";
        let source = make_source_with_preset(store_id, "prose");
        let mut config = make_ingestion_config(store_id);
        config.policy_version = "policy-v2".to_string();
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);

        let mut doc_index = DocumentIndex::new();
        doc_index.upsert(DocumentRecord {
            uri: "https://example.com/doc".to_string(),
            resource_id: "res-1".to_string(),
            source_id: source.id.clone(),
            content_hash: "hash-1".to_string(),
            // Stored under the OLD policy — the run's config above is v2.
            policy_version: "policy-v1".to_string(),
            metadata_hash: "mhash-1".to_string(),
            external_etag: Some("\"abc\"".to_string()),
            external_last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
        });

        let mut callback =
            make_pipeline_callback(&source, &mut doc_index, &store, &embedder, &config);
        let uri = Uri::parse("https://example.com/doc").unwrap();
        let meta = callback.lookup_fetch_metadata(&uri).await;

        assert_eq!(
            meta.etag, None,
            "a policy_version mismatch must suppress the stored ETag — replaying \
             it would let a 304 permanently freeze this resource at the old policy"
        );
        assert_eq!(meta.last_modified, None);
    }

    #[tokio::test]
    async fn lookup_fetch_metadata_returns_empty_when_no_prior_record() {
        let store_id = "store-1";
        let source = make_source_with_preset(store_id, "prose");
        let config = make_ingestion_config(store_id);
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let mut doc_index = DocumentIndex::new();

        let mut callback =
            make_pipeline_callback(&source, &mut doc_index, &store, &embedder, &config);
        let uri = Uri::parse("https://example.com/never-indexed").unwrap();
        let meta = callback.lookup_fetch_metadata(&uri).await;

        assert_eq!(meta.etag, None);
        assert_eq!(meta.last_modified, None);
    }

    // -----------------------------------------------------------------
    // 15. on_validators_refreshed — persisting a 304-refreshed validator
    // -----------------------------------------------------------------

    /// Indexes `resource` (via `index_resource`, so it lands real chunks
    /// in `store`) with `external_etag` overridden to `etag`, and seeds
    /// `doc_index` with the matching `DocumentRecord` — mirroring what
    /// `on_resource`'s own `Written` arm would have stamped, without
    /// going through the callback (these tests exercise
    /// `on_validators_refreshed` directly).
    async fn seed_indexed_with_etag(
        store: &FakeStore,
        embedder: &FakeEmbedder,
        config: &IngestionConfig,
        source: &Source,
        uri: &str,
        text: &str,
        etag: &str,
    ) -> DocumentRecord {
        let mut resource = make_resource(uri, text, &source.id, &config.store_id);
        resource.external_etag = Some(etag.to_string());
        let deps = IndexResourceDeps {
            store,
            embedder,
            config,
        };
        let outcome = index_resource(&resource, source, None, &deps)
            .await
            .expect("seed index must succeed");
        let metadata_hash = match outcome {
            IndexOutcome::Written(_, hash) => hash,
            IndexOutcome::Empty => panic!("seed_indexed_with_etag: must not chunk to empty"),
        };
        DocumentRecord {
            uri: resource.uri.as_str().to_string(),
            resource_id: resource.id.clone(),
            source_id: source.id.clone(),
            content_hash: resource.content_hash.clone(),
            policy_version: config.policy_version.clone(),
            metadata_hash,
            external_etag: resource.external_etag.clone(),
            external_last_modified: resource.external_last_modified.clone(),
        }
    }

    #[tokio::test]
    async fn on_validators_refreshed_with_rotated_etag_updates_stored_row() {
        let store_id = "store-1";
        let source = make_source_with_preset(store_id, "prose");
        let config = make_ingestion_config(store_id);
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let uri_str = "https://example.com/rotating";
        let text = "Stable content that never changes.";

        let mut doc_index = DocumentIndex::new();
        let seeded =
            seed_indexed_with_etag(&store, &embedder, &config, &source, uri_str, text, "v1").await;
        let resource_id = seeded.resource_id.clone();
        doc_index.upsert(seeded);

        let mut callback =
            make_pipeline_callback(&source, &mut doc_index, &store, &embedder, &config);
        let uri = Uri::parse(uri_str).unwrap();
        callback
            .on_validators_refreshed(
                &uri,
                &FetchMetadata {
                    etag: Some("v2".to_string()),
                    last_modified: None,
                },
            )
            .await;

        let chunks = store.get_chunks_for_resource(&resource_id).await.unwrap();
        assert!(!chunks.is_empty());
        assert!(
            chunks
                .iter()
                .all(|c| c.external_etag.as_deref() == Some("v2")),
            "the stored row must carry the rotated ETag the 304 itself reported"
        );

        let cached = callback.doc_index.get(uri_str).unwrap();
        assert_eq!(cached.external_etag.as_deref(), Some("v2"));
    }

    #[tokio::test]
    async fn on_validators_refreshed_bare_304_leaves_stored_row_untouched() {
        let store_id = "store-1";
        let source = make_source_with_preset(store_id, "prose");
        let config = make_ingestion_config(store_id);
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let uri_str = "https://example.com/unchanged";
        let text = "Stable content that never changes.";

        let mut doc_index = DocumentIndex::new();
        let seeded =
            seed_indexed_with_etag(&store, &embedder, &config, &source, uri_str, text, "v1").await;
        let resource_id = seeded.resource_id.clone();
        doc_index.upsert(seeded);

        let mut callback =
            make_pipeline_callback(&source, &mut doc_index, &store, &embedder, &config);
        let uri = Uri::parse(uri_str).unwrap();
        // A bare 304 — both fields None — must be read as "keep what's
        // stored," never "clear it." `process_url` only calls
        // `on_validators_refreshed` when at least one field is `Some`,
        // but this pins the callback's own half of that contract too:
        // it must be a no-op even if called directly with an empty
        // `FetchMetadata`.
        callback
            .on_validators_refreshed(&uri, &FetchMetadata::default())
            .await;

        let chunks = store.get_chunks_for_resource(&resource_id).await.unwrap();
        assert!(
            chunks
                .iter()
                .all(|c| c.external_etag.as_deref() == Some("v1")),
            "a bare 304 must leave the previously stored ETag untouched"
        );
        let cached = callback.doc_index.get(uri_str).unwrap();
        assert_eq!(cached.external_etag.as_deref(), Some("v1"));
    }

    /// A 304 may rotate one validator and say nothing about the other.
    /// RFC 9111 makes silence mean "unchanged", so the field the response
    /// omitted must survive — dropping it would disable half of
    /// conditional GET for that resource on every subsequent run.
    ///
    /// This asserts against the `ResourceRecord` actually handed to the
    /// store rather than against read-back chunk state, because
    /// `external_last_modified` is deliberately not a denormalized
    /// `ChunkRecord` field and so has no per-chunk copy to read back.
    #[tokio::test]
    async fn on_validators_refreshed_preserves_the_validator_a_304_omitted() {
        let store_id = "store-1";
        let source = make_source_with_preset(store_id, "prose");
        let config = make_ingestion_config(store_id);
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let uri_str = "https://example.com/partial";
        let text = "Content whose validators rotate one at a time.";

        let mut doc_index = DocumentIndex::new();
        let mut seeded =
            seed_indexed_with_etag(&store, &embedder, &config, &source, uri_str, text, "v1").await;
        seeded.external_last_modified = Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string());
        doc_index.upsert(seeded);

        let mut callback =
            make_pipeline_callback(&source, &mut doc_index, &store, &embedder, &config);
        let uri = Uri::parse(uri_str).unwrap();

        // A 304 rotating only the ETag must leave Last-Modified alone.
        callback
            .on_validators_refreshed(
                &uri,
                &FetchMetadata {
                    etag: Some("v2".to_string()),
                    last_modified: None,
                },
            )
            .await;

        let updates = store.metadata_updates().await;
        let (_, record) = updates.last().expect("the refresh must reach the store");
        assert_eq!(record.external_etag.as_deref(), Some("v2"));
        assert_eq!(
            record.external_last_modified.as_deref(),
            Some("Wed, 21 Oct 2015 07:28:00 GMT"),
            "a 304 that omitted Last-Modified must not clear the stored one"
        );

        // And the mirror image: rotating only Last-Modified must leave
        // the ETag alone.
        callback
            .on_validators_refreshed(
                &uri,
                &FetchMetadata {
                    etag: None,
                    last_modified: Some("Thu, 22 Oct 2015 07:28:00 GMT".to_string()),
                },
            )
            .await;

        let updates = store.metadata_updates().await;
        let (_, record) = updates.last().expect("the refresh must reach the store");
        assert_eq!(
            record.external_etag.as_deref(),
            Some("v2"),
            "a 304 that omitted ETag must not clear the stored one"
        );
        assert_eq!(
            record.external_last_modified.as_deref(),
            Some("Thu, 22 Oct 2015 07:28:00 GMT")
        );
    }

    /// A well-behaved origin repeats the validator it already issued on
    /// every 304 for unchanged content, so this is the common case, not
    /// an edge one. Writing anyway would rewrite the resource row and
    /// bump `index_updated_at` — publicly visible as
    /// `DocumentInfo.index_updated_at` — on a run that changed nothing.
    ///
    /// Asserted on the store's call log rather than on final state: a
    /// blind rewrite of identical validators leaves the row looking
    /// exactly the same, so a state assertion would pass with the guard
    /// removed.
    #[tokio::test]
    async fn on_validators_refreshed_repeating_the_stored_validators_writes_nothing() {
        let store_id = "store-1";
        let source = make_source_with_preset(store_id, "prose");
        let config = make_ingestion_config(store_id);
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let uri_str = "https://example.com/repeating";
        let text = "Stable content that never changes.";

        let mut doc_index = DocumentIndex::new();
        let mut seeded =
            seed_indexed_with_etag(&store, &embedder, &config, &source, uri_str, text, "v1").await;
        seeded.external_last_modified = Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string());
        doc_index.upsert(seeded);

        let mut callback =
            make_pipeline_callback(&source, &mut doc_index, &store, &embedder, &config);
        let uri = Uri::parse(uri_str).unwrap();
        let outcome = callback
            .on_validators_refreshed(
                &uri,
                &FetchMetadata {
                    etag: Some("v1".to_string()),
                    last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
                },
            )
            .await;

        assert_eq!(outcome, MetadataWriteOutcome::Unchanged);
        assert!(
            store.metadata_updates().await.is_empty(),
            "a 304 repeating the validators already stored must not reach the store"
        );
    }

    /// The half of the guard a `compute_metadata_hash` comparison would
    /// have broken: `external_last_modified` is deliberately not one of
    /// that hash's inputs, so a 304 rotating only `Last-Modified` yields
    /// an identical hash while still needing to be persisted. The guard
    /// compares the validator pair itself for exactly this reason.
    #[tokio::test]
    async fn on_validators_refreshed_rotating_only_last_modified_still_writes() {
        let store_id = "store-1";
        let source = make_source_with_preset(store_id, "prose");
        let config = make_ingestion_config(store_id);
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let uri_str = "https://example.com/lm-only";
        let text = "Stable content that never changes.";

        let mut doc_index = DocumentIndex::new();
        let mut seeded =
            seed_indexed_with_etag(&store, &embedder, &config, &source, uri_str, text, "v1").await;
        seeded.external_last_modified = Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string());
        doc_index.upsert(seeded);

        let mut callback =
            make_pipeline_callback(&source, &mut doc_index, &store, &embedder, &config);
        let uri = Uri::parse(uri_str).unwrap();
        let outcome = callback
            .on_validators_refreshed(
                &uri,
                &FetchMetadata {
                    etag: Some("v1".to_string()),
                    last_modified: Some("Thu, 22 Oct 2015 07:28:00 GMT".to_string()),
                },
            )
            .await;

        assert_eq!(outcome, MetadataWriteOutcome::Written);
        let updates = store.metadata_updates().await;
        let (_, record) = updates.last().expect("the refresh must reach the store");
        assert_eq!(
            record.external_last_modified.as_deref(),
            Some("Thu, 22 Oct 2015 07:28:00 GMT")
        );
    }

    /// `SkipReason::MetadataUpdated` is not a skip. It counts where
    /// `on_resource`'s own metadata-only branch counts, so a metadata
    /// write reads identically whether it arrived with a body or behind
    /// a 304 — and never lands in `docs_skipped` as well, which would
    /// break the partition of `docs_seen`.
    #[tokio::test]
    async fn on_skipped_metadata_updated_counts_as_an_update_not_a_skip() {
        let store_id = "store-1";
        let source = make_source_with_preset(store_id, "prose");
        let config = make_ingestion_config(store_id);
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let mut doc_index = DocumentIndex::new();
        let mut callback =
            make_pipeline_callback(&source, &mut doc_index, &store, &embedder, &config);

        let uri = Uri::parse("https://example.com/refreshed").unwrap();
        callback.on_skipped(&uri, SkipReason::MetadataUpdated).await;

        assert_eq!(callback.result.docs_metadata_updated, 1);
        assert_eq!(callback.result.docs_skipped, 0);
        assert_eq!(callback.result.error_count, 0);
        assert_eq!(callback.result.docs_seen, 1);
        assert!(
            callback.seen.contains("https://example.com/refreshed"),
            "a metadata-updated URI is still alive and must survive the delete-sweep"
        );
    }

    /// The metadata_hash trap, pinned: `external_etag` IS an input to
    /// `compute_metadata_hash`, so rotating it via a 304 refresh without
    /// also refreshing the *cached* `metadata_hash` in `doc_index` would
    /// desync the two. The next metadata-unchanged fetch (a normal 200
    /// whose own reported ETag now matches what the 304 already
    /// rotated to) would then see a spurious mismatch and route through
    /// a needless metadata-only update — churn this test would catch as
    /// a wrongly nonzero `docs_metadata_updated`.
    #[tokio::test]
    async fn on_validators_refreshed_keeps_metadata_hash_in_sync_no_churn_next_run() {
        let store_id = "store-1";
        let source = make_source_with_preset(store_id, "prose");
        let config = make_ingestion_config(store_id);
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let uri_str = "https://example.com/no-churn";
        let text = "Stable content that never changes.";

        let mut doc_index = DocumentIndex::new();
        let seeded =
            seed_indexed_with_etag(&store, &embedder, &config, &source, uri_str, text, "v1").await;
        doc_index.upsert(seeded);

        let mut callback =
            make_pipeline_callback(&source, &mut doc_index, &store, &embedder, &config);
        let uri = Uri::parse(uri_str).unwrap();
        callback
            .on_validators_refreshed(
                &uri,
                &FetchMetadata {
                    etag: Some("v2".to_string()),
                    last_modified: None,
                },
            )
            .await;

        // A subsequent run's ordinary 200 fetch: identical content, and
        // the origin now consistently reports the SAME "v2" ETag the
        // 304 already rotated to.
        let mut resource_next_run = make_resource(uri_str, text, &source.id, store_id);
        resource_next_run.external_etag = Some("v2".to_string());
        callback.on_resource(resource_next_run).await.unwrap();

        assert_eq!(
            callback.result.docs_skipped, 1,
            "content and metadata are both unchanged relative to the refreshed \
             state — this must be an ordinary skip"
        );
        assert_eq!(
            callback.result.docs_metadata_updated, 0,
            "a correctly-synced metadata_hash must not churn a metadata-only \
             update on the very next unchanged fetch"
        );
    }

    // -----------------------------------------------------------------
    // Feed liveness sweep (specs/04-search-pipeline.md §1 "Aged-out
    // feed entries: the liveness sweep")
    // -----------------------------------------------------------------
    mod feed_liveness_sweep {
        use super::*;
        use crate::store::{MetadataFilter, ResourceRecord, SearchResult, StoreStats};

        #[derive(Debug, Clone)]
        struct LivenessRow {
            resource_id: String,
            uri: String,
            external_id: Option<String>,
            external_etag: Option<String>,
            external_last_modified: Option<String>,
            last_checked_at: Option<String>,
        }

        /// A discovered feed entry: stamped with the entry's own id, as
        /// `FeedIngestor` stamps every entry it yields.
        fn row(resource_id: &str, uri: &str, last_checked_at: Option<&str>) -> LivenessRow {
            LivenessRow {
                resource_id: resource_id.to_string(),
                uri: uri.to_string(),
                external_id: Some(format!("urn:entry:{resource_id}")),
                external_etag: None,
                external_last_modified: None,
                last_checked_at: last_checked_at.map(str::to_string),
            }
        }

        /// The feed's own document, as single-document mode
        /// (`fetch_full_content: false`) stores it: a `feed` resource under
        /// the feed URL, and the only one carrying no `external_id`.
        fn feed_root_row(
            resource_id: &str,
            uri: &str,
            last_checked_at: Option<&str>,
        ) -> LivenessRow {
            LivenessRow {
                external_id: None,
                ..row(resource_id, uri, last_checked_at)
            }
        }

        /// Same, but with a validator already stored — the state every
        /// "what does the sweep write back" assertion needs, since a
        /// row with no stored validator cannot distinguish "kept what
        /// was there" from "wrote nothing".
        fn row_with_etag(
            resource_id: &str,
            uri: &str,
            last_checked_at: Option<&str>,
            etag: &str,
        ) -> LivenessRow {
            LivenessRow {
                external_etag: Some(etag.to_string()),
                ..row(resource_id, uri, last_checked_at)
            }
        }

        /// A minimal `RetrievalStore` double for the liveness sweep: an
        /// in-memory candidate table plus call recorders, so tests can
        /// assert both the sweep's *decisions* (delete vs. touch vs.
        /// leave alone) and its *restraint* (never queries the store at
        /// all when a guard suppresses it, never fetches a candidate the
        /// recheck floor or `seen` rules out).
        /// `(resource_id, etag, last_modified)` recorded per
        /// `touch_resource_liveness` call.
        type TouchCall = (String, Option<String>, Option<String>);

        struct LivenessStore {
            rows: tokio::sync::Mutex<Vec<LivenessRow>>,
            delete_calls: tokio::sync::Mutex<Vec<String>>,
            touch_calls: tokio::sync::Mutex<Vec<TouchCall>>,
            list_calls: std::sync::atomic::AtomicUsize,
            /// The `limit` the last `list_stale_feed_resources` call
            /// asked for — what pins the over-fetch arithmetic.
            last_limit: std::sync::atomic::AtomicUsize,
            /// Resource IDs `touch_resource_liveness` fails for —
            /// simulates a concurrent delete racing the probe, without
            /// having to actually race one.
            fail_touch_for: std::collections::HashSet<String>,
        }

        impl LivenessStore {
            fn new(rows: Vec<LivenessRow>) -> Self {
                Self {
                    rows: tokio::sync::Mutex::new(rows),
                    delete_calls: tokio::sync::Mutex::new(Vec::new()),
                    touch_calls: tokio::sync::Mutex::new(Vec::new()),
                    list_calls: std::sync::atomic::AtomicUsize::new(0),
                    last_limit: std::sync::atomic::AtomicUsize::new(0),
                    fail_touch_for: std::collections::HashSet::new(),
                }
            }

            fn new_with_touch_failure(rows: Vec<LivenessRow>, fail_for: &str) -> Self {
                Self {
                    fail_touch_for: std::iter::once(fail_for.to_string()).collect(),
                    ..Self::new(rows)
                }
            }

            fn list_call_count(&self) -> usize {
                self.list_calls.load(std::sync::atomic::Ordering::SeqCst)
            }

            /// The `limit` the last candidate query asked for, or
            /// `None` if no query has run.
            fn last_query_limit(&self) -> Option<usize> {
                match self.last_limit.load(std::sync::atomic::Ordering::SeqCst) {
                    0 => None,
                    n => Some(n),
                }
            }

            /// The stored row for `resource_id`, as the sweep left it.
            async fn row_state(&self, resource_id: &str) -> LivenessRow {
                self.rows
                    .lock()
                    .await
                    .iter()
                    .find(|r| r.resource_id == resource_id)
                    .expect("row must still exist")
                    .clone()
            }
        }

        #[async_trait::async_trait]
        impl RetrievalStore for LivenessStore {
            async fn upsert_chunks(&self, _records: Vec<ChunkRecord>) -> Result<usize, Error> {
                Ok(0)
            }

            async fn delete_by_resource(&self, resource_id: &str) -> Result<usize, Error> {
                self.delete_calls.lock().await.push(resource_id.to_string());
                let mut rows = self.rows.lock().await;
                let before = rows.len();
                rows.retain(|r| r.resource_id != resource_id);
                Ok(before - rows.len())
            }

            async fn delete_by_store(&self, _store_id: &str) -> Result<usize, Error> {
                Ok(0)
            }

            async fn dense_search(
                &self,
                _query_vector: &[f32],
                _limit: usize,
                _filters: &[MetadataFilter],
            ) -> Result<Vec<SearchResult>, Error> {
                Ok(Vec::new())
            }

            async fn bm25_search(
                &self,
                _query_text: &str,
                _limit: usize,
                _filters: &[MetadataFilter],
            ) -> Result<Vec<SearchResult>, Error> {
                Ok(Vec::new())
            }

            async fn stats(&self) -> Result<StoreStats, Error> {
                Ok(StoreStats {
                    chunk_count: 0,
                    document_count: 0,
                })
            }

            async fn get_chunk(&self, _chunk_id: &str) -> Result<Option<ChunkRecord>, Error> {
                Ok(None)
            }

            async fn get_chunks_for_resource(
                &self,
                _resource_id: &str,
            ) -> Result<Vec<ChunkRecord>, Error> {
                Ok(Vec::new())
            }

            async fn list_indexed_documents(&self) -> Result<Vec<DocumentRecord>, Error> {
                Ok(Vec::new())
            }

            async fn update_resource_metadata(
                &self,
                _store_id: &str,
                _resource_id: &str,
                _record: &ResourceRecord,
            ) -> Result<(), Error> {
                unimplemented!("not exercised by the liveness sweep")
            }

            async fn get_resource_record(
                &self,
                _store_id: &str,
                _resource_id: &str,
            ) -> Result<Option<ResourceRecord>, Error> {
                unimplemented!("not exercised by the liveness sweep")
            }

            async fn list_stale_feed_resources(
                &self,
                _store_id: &str,
                _source_id: &str,
                checked_before: &str,
                limit: usize,
            ) -> Result<Vec<StaleFeedResource>, Error> {
                self.list_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.last_limit
                    .store(limit, std::sync::atomic::Ordering::SeqCst);
                let rows = self.rows.lock().await;
                let mut candidates: Vec<&LivenessRow> = rows
                    .iter()
                    // A URI carrying a fragment (a link-less entry's
                    // synthetic `{feed_url}#entry:{id}`) is never a
                    // candidate — mirrors `store-libsql`'s
                    // `instr(uri, '#') = 0` and the `uri LIKE
                    // 'http(s)://%'` scheme filter in the real query.
                    .filter(|r| !r.uri.contains('#'))
                    .filter(|r| r.uri.starts_with("http://") || r.uri.starts_with("https://"))
                    // Only discovered entries. The feed's own document
                    // carries no `external_id` — mirrors the real query's
                    // `external_id IS NOT NULL`.
                    .filter(|r| r.external_id.is_some())
                    .filter(|r| {
                        r.last_checked_at
                            .as_deref()
                            .is_none_or(|checked| checked < checked_before)
                    })
                    .collect();
                // `None` sorts before `Some`, matching SQLite's plain
                // `ORDER BY last_checked_at ASC` (NULL first) — see
                // `store-libsql`'s `list_stale_feed_resources` for the
                // real query this mirrors.
                candidates.sort_by(|a, b| a.last_checked_at.cmp(&b.last_checked_at));
                Ok(candidates
                    .into_iter()
                    .take(limit)
                    .map(|r| StaleFeedResource {
                        resource_id: r.resource_id.clone(),
                        uri: r.uri.clone(),
                        external_etag: r.external_etag.clone(),
                        external_last_modified: r.external_last_modified.clone(),
                    })
                    .collect())
            }

            async fn touch_resource_liveness(
                &self,
                _store_id: &str,
                resource_id: &str,
                etag: Option<&str>,
                last_modified: Option<&str>,
            ) -> Result<(), Error> {
                self.touch_calls.lock().await.push((
                    resource_id.to_string(),
                    etag.map(str::to_string),
                    last_modified.map(str::to_string),
                ));
                if self.fail_touch_for.contains(resource_id) {
                    return Err(Error::ResourceNotFound {
                        id: resource_id.to_string(),
                    });
                }
                let mut rows = self.rows.lock().await;
                if let Some(r) = rows.iter_mut().find(|r| r.resource_id == resource_id) {
                    r.external_etag = etag.map(str::to_string);
                    r.external_last_modified = last_modified.map(str::to_string);
                    r.last_checked_at = Some(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true));
                }
                Ok(())
            }
        }

        #[derive(Clone, Copy)]
        enum ScriptedFetchOutcome {
            Gone,
            NotModified,
            Downloaded,
            Blocked,
            TransportError,
        }

        /// Records every URL fetched, in call order — the recheck-floor
        /// and batch-cap tests below assert directly on `calls`.
        struct ScriptedFetcher {
            default_outcome: ScriptedFetchOutcome,
            calls: tokio::sync::Mutex<Vec<String>>,
            /// The `FetchMetadata` each call received, in call order —
            /// what proves *which* validator a later probe replays.
            replayed: tokio::sync::Mutex<Vec<FetchMetadata>>,
        }

        impl ScriptedFetcher {
            fn new(default_outcome: ScriptedFetchOutcome) -> Self {
                Self {
                    default_outcome,
                    calls: tokio::sync::Mutex::new(Vec::new()),
                    replayed: tokio::sync::Mutex::new(Vec::new()),
                }
            }
        }

        #[async_trait::async_trait]
        impl UrlFetcher for ScriptedFetcher {
            async fn fetch(
                &self,
                url: &str,
                metadata: &FetchMetadata,
            ) -> Result<FetchResult, Error> {
                self.calls.lock().await.push(url.to_string());
                self.replayed.lock().await.push(metadata.clone());
                match self.default_outcome {
                    ScriptedFetchOutcome::Gone => Ok(FetchResult::Gone),
                    ScriptedFetchOutcome::NotModified => Ok(FetchResult::NotModified {
                        etag: None,
                        last_modified: None,
                    }),
                    ScriptedFetchOutcome::Downloaded => Ok(FetchResult::Downloaded {
                        bytes: Vec::new(),
                        content_type: None,
                        etag: Some("\"fresh\"".to_string()),
                        last_modified: None,
                        final_url: None,
                    }),
                    ScriptedFetchOutcome::Blocked => Ok(FetchResult::Blocked),
                    ScriptedFetchOutcome::TransportError => Err(Error::Internal {
                        message: "simulated transport error".to_string(),
                        correlation_id: "liveness_sweep_test_fetch_error".to_string(),
                    }),
                }
            }
        }

        fn old_timestamp() -> String {
            "2020-01-01T00:00:00Z".to_string()
        }

        // -------------------------------------------------------------
        // Per-candidate outcomes
        // -------------------------------------------------------------

        #[tokio::test]
        async fn gone_candidate_is_deleted_and_counted() {
            let store = LivenessStore::new(vec![row(
                "r1",
                "https://a.example.com/",
                Some(&old_timestamp()),
            )]);
            let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Gone);
            let mut doc_index = DocumentIndex::new();
            let seen = std::collections::HashSet::new();
            let mut result = IngestionResult::default();

            run_feed_liveness_sweep(
                "src-1",
                "store-1",
                None,
                &seen,
                &mut doc_index,
                &store,
                &fetcher,
                &mut result,
            )
            .await
            .unwrap();

            assert_eq!(result.docs_deleted, 1);
            assert_eq!(result.feed_entries_liveness_checked, 1);
            assert_eq!(*store.delete_calls.lock().await, vec!["r1".to_string()]);
            assert!(store.touch_calls.lock().await.is_empty());
        }

        #[tokio::test]
        async fn not_modified_candidate_is_touched_not_deleted() {
            let store = LivenessStore::new(vec![row(
                "r1",
                "https://a.example.com/",
                Some(&old_timestamp()),
            )]);
            let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::NotModified);
            let mut doc_index = DocumentIndex::new();
            let seen = std::collections::HashSet::new();
            let mut result = IngestionResult::default();

            run_feed_liveness_sweep(
                "src-1",
                "store-1",
                None,
                &seen,
                &mut doc_index,
                &store,
                &fetcher,
                &mut result,
            )
            .await
            .unwrap();

            assert_eq!(result.docs_deleted, 0);
            assert_eq!(result.feed_entries_liveness_checked, 1);
            assert!(store.delete_calls.lock().await.is_empty());
            assert_eq!(store.touch_calls.lock().await.len(), 1);
        }

        /// A 200 is touched (validators + `last_checked_at` refreshed),
        /// never deleted, and — the point of this test — never
        /// re-indexed: nothing in this test's `LivenessStore` exposes an
        /// `upsert_chunks`/`upsert_chunks_and_blocks` write path that
        /// records a call, so a passing assertion on `touch_calls` alone
        /// (no other store method touched) already proves no re-index
        /// happened.
        /// Run the sweep once over `store` with `fetcher`, no seen-set
        /// and no configured refresh interval — the shape almost every
        /// single-candidate test below wants.
        async fn sweep_once(store: &LivenessStore, fetcher: &ScriptedFetcher) -> IngestionResult {
            let mut doc_index = DocumentIndex::new();
            let seen = std::collections::HashSet::new();
            let mut result = IngestionResult::default();
            run_feed_liveness_sweep(
                "src-1",
                "store-1",
                None,
                &seen,
                &mut doc_index,
                store,
                fetcher,
                &mut result,
            )
            .await
            .unwrap();
            result
        }

        /// A `200` refreshes the clock and **nothing else**: the
        /// response's own validators are discarded along with the body
        /// they describe. Caching them would leave the resource pointing
        /// at a representation this store never indexed, so a later
        /// probe would answer 304 — and if the entry ever re-entered the
        /// feed window, that 304 would suppress the reindex of the
        /// changed content indefinitely.
        #[tokio::test]
        async fn downloaded_candidate_keeps_its_stored_validators() {
            let store = LivenessStore::new(vec![row_with_etag(
                "r1",
                "https://a.example.com/",
                Some(&old_timestamp()),
                "\"stored\"",
            )]);
            let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Downloaded);

            let result = sweep_once(&store, &fetcher).await;

            assert_eq!(result.docs_deleted, 0);
            assert!(store.delete_calls.lock().await.is_empty());
            let touches = store.touch_calls.lock().await.clone();
            assert_eq!(touches.len(), 1);
            assert_eq!(touches[0].0, "r1");
            assert_eq!(
                touches[0].1.as_deref(),
                Some("\"stored\""),
                "the response's own \"fresh\" ETag describes a body the sweep threw \
                 away, so it must never be stored"
            );

            let after = store.row_state("r1").await;
            assert_eq!(after.external_etag.as_deref(), Some("\"stored\""));
            assert!(
                after.last_checked_at.as_deref() > Some(old_timestamp().as_str()),
                "the probe clock is the one thing a 200 does move"
            );
        }

        /// The consequence, stated as the loop it breaks: a second probe
        /// of the same candidate replays the **old** validator, so the
        /// origin keeps answering 200 with the changed content rather
        /// than 304ing against something never indexed.
        #[tokio::test]
        async fn a_second_probe_replays_the_pre_probe_validator() {
            let store = LivenessStore::new(vec![row_with_etag(
                "r1",
                "https://a.example.com/",
                Some(&old_timestamp()),
                "\"stored\"",
            )]);
            let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Downloaded);

            sweep_once(&store, &fetcher).await;
            // Wind the clock back so the recheck floor lets it through
            // again, without sleeping out a real 24h window.
            store.rows.lock().await[0].last_checked_at = Some(old_timestamp());
            sweep_once(&store, &fetcher).await;

            let replayed = fetcher.replayed.lock().await.clone();
            assert_eq!(replayed.len(), 2);
            assert_eq!(
                replayed[1].etag.as_deref(),
                Some("\"stored\""),
                "replaying the response's fresh ETag here would 304 forever \
                 against content this store never indexed"
            );
        }

        /// `Blocked` is evidence of nothing about the entry, so nothing
        /// about its content, metadata or validators moves — but the
        /// clock does. The candidate query is oldest-first, so a stuck
        /// candidate that kept its old timestamp would lead every
        /// subsequent query and starve the rest of the batch.
        #[tokio::test]
        async fn blocked_candidate_advances_only_the_probe_clock() {
            let store = LivenessStore::new(vec![row_with_etag(
                "r1",
                "https://a.example.com/",
                Some(&old_timestamp()),
                "\"stored\"",
            )]);
            let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Blocked);

            let result = sweep_once(&store, &fetcher).await;

            assert_eq!(result.docs_deleted, 0);
            assert_eq!(
                result.feed_entries_liveness_checked, 1,
                "still counted as probed"
            );
            assert!(store.delete_calls.lock().await.is_empty());
            let touches = store.touch_calls.lock().await.clone();
            assert_eq!(touches.len(), 1);
            assert_eq!(touches[0].1.as_deref(), Some("\"stored\""));
            let after = store.row_state("r1").await;
            assert_eq!(after.external_etag.as_deref(), Some("\"stored\""));
            assert!(after.last_checked_at.as_deref() > Some(old_timestamp().as_str()));
        }

        #[tokio::test]
        async fn transport_error_advances_only_the_probe_clock() {
            let store = LivenessStore::new(vec![row_with_etag(
                "r1",
                "https://a.example.com/",
                Some(&old_timestamp()),
                "\"stored\"",
            )]);
            let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::TransportError);

            let result = sweep_once(&store, &fetcher).await;

            assert_eq!(result.docs_deleted, 0);
            assert_eq!(result.feed_entries_liveness_checked, 1);
            assert!(store.delete_calls.lock().await.is_empty());
            let touches = store.touch_calls.lock().await.clone();
            assert_eq!(touches.len(), 1);
            assert_eq!(touches[0].1.as_deref(), Some("\"stored\""));
            let after = store.row_state("r1").await;
            assert_eq!(after.external_etag.as_deref(), Some("\"stored\""));
            assert!(after.last_checked_at.as_deref() > Some(old_timestamp().as_str()));
        }

        /// Starvation, run end to end: a source whose whole batch is
        /// permanently blocked must not re-probe the same
        /// `FEED_LIVENESS_BATCH_LIMIT` candidates forever. Because every
        /// attempt advances the clock, the second run reaches the ones
        /// the first could not.
        #[tokio::test]
        async fn a_blocked_batch_does_not_starve_the_candidates_behind_it() {
            let rows: Vec<LivenessRow> = (0..30)
                .map(|i| {
                    row_with_etag(
                        &format!("r{i}"),
                        &format!("https://a.example.com/{i}"),
                        // Distinct, ordered timestamps so the query has
                        // an unambiguous "oldest first" to work from.
                        Some(&format!("2020-01-01T00:00:{i:02}Z")),
                        "\"stored\"",
                    )
                })
                .collect();
            let store = LivenessStore::new(rows);
            let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Blocked);

            sweep_once(&store, &fetcher).await;
            sweep_once(&store, &fetcher).await;

            let probed: std::collections::HashSet<String> =
                fetcher.calls.lock().await.iter().cloned().collect();
            assert!(
                probed.len() > FEED_LIVENESS_BATCH_LIMIT,
                "the second run must reach candidates the first could not; \
                 got {} distinct URLs across two runs",
                probed.len()
            );
            // Nothing but the clock moved for any of them.
            for i in 0..30 {
                let after = store.row_state(&format!("r{i}")).await;
                assert_eq!(after.external_etag.as_deref(), Some("\"stored\""));
            }
        }

        /// The batch cap counts candidates *probed*, not rows returned.
        /// The store query cannot see the run's seen-set, so a SQL-side
        /// `LIMIT 25` over a window whose freshly-observed entries sort
        /// oldest would hand back 25 already-seen rows and the sweep
        /// would probe nothing at all — permanently.
        #[tokio::test]
        async fn a_seen_heavy_window_still_yields_real_probes() {
            let rows: Vec<LivenessRow> = (0..30)
                .map(|i| {
                    row(
                        &format!("r{i}"),
                        &format!("https://a.example.com/{i}"),
                        Some(&format!("2020-01-01T00:00:{i:02}Z")),
                    )
                })
                .collect();
            let store = LivenessStore::new(rows);
            let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::NotModified);
            // The 25 oldest — exactly what a SQL-side cap would return.
            let seen: std::collections::HashSet<String> = (0..FEED_LIVENESS_BATCH_LIMIT)
                .map(|i| format!("https://a.example.com/{i}"))
                .collect();
            let mut doc_index = DocumentIndex::new();
            let mut result = IngestionResult::default();

            run_feed_liveness_sweep(
                "src-1",
                "store-1",
                None,
                &seen,
                &mut doc_index,
                &store,
                &fetcher,
                &mut result,
            )
            .await
            .unwrap();

            let calls = fetcher.calls.lock().await.clone();
            assert_eq!(
                calls.len(),
                5,
                "the five aged-out entries behind the seen-set must be probed: {calls:?}"
            );
            assert!(
                calls.iter().all(|u| !seen.contains(u)),
                "no entry this run already observed may be probed"
            );
        }

        /// The over-fetch has a ceiling, so a pathologically large
        /// seen-set cannot turn one query into an unbounded one.
        #[tokio::test]
        async fn the_over_fetch_is_capped() {
            let store = LivenessStore::new(vec![row(
                "r1",
                "https://a.example.com/",
                Some(&old_timestamp()),
            )]);
            let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::NotModified);
            let seen: std::collections::HashSet<String> = (0..10_000)
                .map(|i| format!("https://seen.example.com/{i}"))
                .collect();
            let mut doc_index = DocumentIndex::new();
            let mut result = IngestionResult::default();

            run_feed_liveness_sweep(
                "src-1",
                "store-1",
                None,
                &seen,
                &mut doc_index,
                &store,
                &fetcher,
                &mut result,
            )
            .await
            .unwrap();

            assert_eq!(
                store.last_query_limit(),
                Some(FEED_LIVENESS_OVERFETCH_CAP),
                "the seen-set's own size must never become the query's limit"
            );
        }

        /// A per-candidate `touch_resource_liveness` failure (e.g. a
        /// concurrent delete racing the probe) must not abort the whole
        /// source and discard the stats already computed for candidates
        /// processed alongside it — the transport-error and `Blocked`
        /// arms beside this one already handle their own failures per
        /// candidate; this must be consistent with them.
        #[tokio::test]
        async fn touch_resource_liveness_failure_does_not_abort_remaining_candidates() {
            let store = LivenessStore::new_with_touch_failure(
                vec![
                    row(
                        "r-fails",
                        "https://a.example.com/",
                        Some("2020-01-01T00:00:00Z"),
                    ),
                    row(
                        "r-succeeds",
                        "https://b.example.com/",
                        Some("2020-01-02T00:00:00Z"),
                    ),
                ],
                "r-fails",
            );
            let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::NotModified);
            let mut doc_index = DocumentIndex::new();
            let seen = std::collections::HashSet::new();
            let mut result = IngestionResult::default();

            run_feed_liveness_sweep(
                "src-1",
                "store-1",
                None,
                &seen,
                &mut doc_index,
                &store,
                &fetcher,
                &mut result,
            )
            .await
            .unwrap();

            assert_eq!(
                fetcher.calls.lock().await.len(),
                2,
                "the failing candidate must not stop the loop from reaching the next one"
            );
            assert_eq!(
                result.feed_entries_liveness_checked, 2,
                "both candidates must still be counted, including the one whose touch \
                 failed"
            );
            assert_eq!(
                store.touch_calls.lock().await.len(),
                2,
                "touch_resource_liveness must still be attempted for both candidates"
            );
        }

        // -------------------------------------------------------------
        // Throttle: recheck floor and `seen`
        // -------------------------------------------------------------

        #[tokio::test]
        async fn candidate_newer_than_the_recheck_floor_is_never_fetched() {
            // Checked a minute ago — well inside the bare 24h floor
            // (`refresh_interval_secs: None`).
            let recent = (Utc::now() - chrono::Duration::seconds(60))
                .to_rfc3339_opts(SecondsFormat::Secs, true);
            let store =
                LivenessStore::new(vec![row("r1", "https://a.example.com/", Some(&recent))]);
            let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Gone);
            let mut doc_index = DocumentIndex::new();
            let seen = std::collections::HashSet::new();
            let mut result = IngestionResult::default();

            run_feed_liveness_sweep(
                "src-1",
                "store-1",
                None,
                &seen,
                &mut doc_index,
                &store,
                &fetcher,
                &mut result,
            )
            .await
            .unwrap();

            assert!(
                fetcher.calls.lock().await.is_empty(),
                "a resource checked well inside the recheck floor must never be fetched"
            );
            assert_eq!(result.feed_entries_liveness_checked, 0);
        }

        /// A configured `refresh_interval_secs` above the bare 24h floor
        /// raises the effective floor — a resource checked 25h ago (past
        /// the bare floor, but not past a configured 30-day one) must
        /// still not be fetched.
        #[tokio::test]
        async fn configured_refresh_interval_raises_the_recheck_floor_above_24h() {
            let twenty_five_hours_ago = (Utc::now() - chrono::Duration::hours(25))
                .to_rfc3339_opts(SecondsFormat::Secs, true);
            let store = LivenessStore::new(vec![row(
                "r1",
                "https://a.example.com/",
                Some(&twenty_five_hours_ago),
            )]);
            let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Gone);
            let mut doc_index = DocumentIndex::new();
            let seen = std::collections::HashSet::new();
            let mut result = IngestionResult::default();

            run_feed_liveness_sweep(
                "src-1",
                "store-1",
                Some(30 * 24 * 60 * 60), // 30 days
                &seen,
                &mut doc_index,
                &store,
                &fetcher,
                &mut result,
            )
            .await
            .unwrap();

            assert!(
                fetcher.calls.lock().await.is_empty(),
                "a 30-day configured refresh interval must raise the floor above the bare 24h default"
            );
        }

        /// A `refresh_interval_secs` above `i64::MAX` must not overflow
        /// the `as i64` cast the recheck-floor computation used to use: a
        /// wrapped-negative value would push `checked_before` into the
        /// future, making every resource a candidate — the opposite of
        /// the throttle's purpose. A resource checked one minute ago must
        /// stay well inside any correctly computed floor regardless of
        /// how large the configured interval is.
        #[tokio::test]
        async fn recheck_floor_with_u64_max_refresh_interval_never_lands_in_the_future() {
            let one_minute_ago = (Utc::now() - chrono::Duration::seconds(60))
                .to_rfc3339_opts(SecondsFormat::Secs, true);
            let store = LivenessStore::new(vec![row(
                "r1",
                "https://a.example.com/",
                Some(&one_minute_ago),
            )]);
            let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Gone);
            let mut doc_index = DocumentIndex::new();
            let seen = std::collections::HashSet::new();
            let mut result = IngestionResult::default();

            run_feed_liveness_sweep(
                "src-1",
                "store-1",
                Some(u64::MAX),
                &seen,
                &mut doc_index,
                &store,
                &fetcher,
                &mut result,
            )
            .await
            .unwrap();

            assert!(
                fetcher.calls.lock().await.is_empty(),
                "an overflowing refresh_interval_secs must never push checked_before into \
                 the future — that would make every resource a candidate"
            );
        }

        /// `refresh_interval_secs: Some(0)` must not drop the recheck
        /// floor below the bare 24h minimum — the `.max(...)` call
        /// guards this, but only if the value it is maxed against
        /// actually reaches `checked_before` afterward.
        #[tokio::test]
        async fn recheck_floor_with_zero_configured_refresh_interval_never_drops_below_24h() {
            let twenty_three_hours_ago = (Utc::now() - chrono::Duration::hours(23))
                .to_rfc3339_opts(SecondsFormat::Secs, true);
            let store = LivenessStore::new(vec![row(
                "r1",
                "https://a.example.com/",
                Some(&twenty_three_hours_ago),
            )]);
            let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Gone);
            let mut doc_index = DocumentIndex::new();
            let seen = std::collections::HashSet::new();
            let mut result = IngestionResult::default();

            run_feed_liveness_sweep(
                "src-1",
                "store-1",
                Some(0),
                &seen,
                &mut doc_index,
                &store,
                &fetcher,
                &mut result,
            )
            .await
            .unwrap();

            assert!(
                fetcher.calls.lock().await.is_empty(),
                "a configured refresh_interval_secs of 0 must not drop the recheck floor \
                 below the bare 24h minimum"
            );
        }

        #[tokio::test]
        async fn a_candidate_still_in_this_runs_seen_set_is_never_fetched() {
            let store = LivenessStore::new(vec![row(
                "r1",
                "https://a.example.com/",
                Some(&old_timestamp()),
            )]);
            let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Gone);
            let mut doc_index = DocumentIndex::new();
            let mut seen = std::collections::HashSet::new();
            seen.insert("https://a.example.com/".to_string());
            let mut result = IngestionResult::default();

            run_feed_liveness_sweep(
                "src-1",
                "store-1",
                None,
                &seen,
                &mut doc_index,
                &store,
                &fetcher,
                &mut result,
            )
            .await
            .unwrap();

            assert!(
                fetcher.calls.lock().await.is_empty(),
                "a candidate this run's own ingestion pass already observed must not be probed"
            );
            assert_eq!(result.feed_entries_liveness_checked, 0);
        }

        // -------------------------------------------------------------
        // Batch cap
        // -------------------------------------------------------------

        #[tokio::test]
        async fn over_cap_batch_processes_only_the_cap_oldest_first() {
            let mut rows = Vec::new();
            let mut expected_order = Vec::new();
            for i in 0..(FEED_LIVENESS_BATCH_LIMIT + 5) {
                let resource_id = format!("r{i:03}");
                let uri = format!("https://{i:03}.example.com/");
                // Strictly increasing timestamps -> strictly oldest-first
                // order is unambiguous.
                let checked_at = format!("2020-01-{:02}T00:00:00Z", (i % 28) + 1);
                rows.push(row(&resource_id, &uri, Some(&checked_at)));
                expected_order.push((checked_at, uri));
            }
            expected_order.sort();
            let expected_uris: Vec<String> = expected_order
                .into_iter()
                .take(FEED_LIVENESS_BATCH_LIMIT)
                .map(|(_, uri)| uri)
                .collect();

            let store = LivenessStore::new(rows);
            let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::NotModified);
            let mut doc_index = DocumentIndex::new();
            let seen = std::collections::HashSet::new();
            let mut result = IngestionResult::default();

            run_feed_liveness_sweep(
                "src-1",
                "store-1",
                None,
                &seen,
                &mut doc_index,
                &store,
                &fetcher,
                &mut result,
            )
            .await
            .unwrap();

            let calls = fetcher.calls.lock().await.clone();
            assert_eq!(calls.len(), FEED_LIVENESS_BATCH_LIMIT);
            assert_eq!(
                calls, expected_uris,
                "the batch cap must keep exactly the oldest N candidates, in oldest-first order"
            );
            assert_eq!(
                result.feed_entries_liveness_checked as usize,
                FEED_LIVENESS_BATCH_LIMIT
            );
        }

        // -------------------------------------------------------------
        // Fragment URIs (link-less entries)
        // -------------------------------------------------------------

        /// A link-less entry's synthetic `{feed_url}#entry:{id}` URI must
        /// never be probed, even when it is the oldest (never-checked)
        /// candidate and the feed root would answer 404: HTTP never sends
        /// a fragment on the wire, so probing it verbatim would actually
        /// request the feed root, and a positive `Gone` there must not
        /// delete the entry's resource.
        #[tokio::test]
        async fn fragment_uri_candidate_is_never_fetched_or_deleted() {
            let store = LivenessStore::new(vec![row(
                "r-fragment",
                "https://feed.example.com/feed.xml#entry:entry-1",
                None, // never-checked — would otherwise sort first
            )]);
            let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Gone);
            let mut doc_index = DocumentIndex::new();
            let seen = std::collections::HashSet::new();
            let mut result = IngestionResult::default();

            run_feed_liveness_sweep(
                "src-1",
                "store-1",
                None,
                &seen,
                &mut doc_index,
                &store,
                &fetcher,
                &mut result,
            )
            .await
            .unwrap();

            assert!(
                fetcher.calls.lock().await.is_empty(),
                "a fragment URI must never be fetched — a fragment is never sent on \
                 the wire, so the request would actually hit the feed root"
            );
            assert_eq!(result.feed_entries_liveness_checked, 0);
            assert_eq!(
                result.docs_deleted, 0,
                "the entry's resource must not be deleted on a signal that has \
                 nothing to do with it"
            );
            assert_eq!(
                store.rows.lock().await.len(),
                1,
                "the resource must still exist in the store after the sweep"
            );
        }

        /// The feed's own document, in single-document mode, is a `feed`
        /// resource under the feed URL — so it matches every candidate
        /// predicate except the one that exists for it. A 404/410 on the
        /// feed URL would otherwise delete the source's entire index through
        /// a mechanism written to prune a single entry.
        #[tokio::test]
        async fn feed_root_candidate_is_never_fetched_or_deleted() {
            let store = LivenessStore::new(vec![feed_root_row(
                "r-feed-root",
                "https://feed.example.com/feed.xml",
                None, // never-checked — would otherwise sort first
            )]);
            let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Gone);
            let mut doc_index = DocumentIndex::new();
            let seen = std::collections::HashSet::new();
            let mut result = IngestionResult::default();

            run_feed_liveness_sweep(
                "src-1",
                "store-1",
                None,
                &seen,
                &mut doc_index,
                &store,
                &fetcher,
                &mut result,
            )
            .await
            .unwrap();

            assert!(
                fetcher.calls.lock().await.is_empty(),
                "the feed's own document must never be probed by the entry sweep"
            );
            assert_eq!(result.feed_entries_liveness_checked, 0);
            assert_eq!(
                result.docs_deleted, 0,
                "a 404 on the feed URL must not delete a single-document index"
            );
            assert_eq!(store.rows.lock().await.len(), 1);
        }

        // -------------------------------------------------------------
        // Guards (run through the full `run_source_ingestion`, since
        // both guards live there, not inside `run_feed_liveness_sweep`
        // itself)
        // -------------------------------------------------------------

        fn make_feed_source(store_id: &str) -> Source {
            Source {
                id: new_ulid(),
                store_id: store_id.to_string(),
                kind: SourceKind::Feed,
                spec: SourceSpec::Feed {
                    url: "https://feed.example.com/feed.xml".to_string(),
                    max_entries: None,
                    fetch_full_content: true,
                    refresh_interval_secs: None,
                },
                source_preset: "prose".to_string(),
            }
        }

        fn seed_doc_index_owned_by(doc_index: &mut DocumentIndex, source_id: &str, uri: &str) {
            doc_index.upsert(DocumentRecord {
                uri: uri.to_string(),
                resource_id: format!("{uri}-resource"),
                source_id: source_id.to_string(),
                content_hash: "hash".to_string(),
                policy_version: "policy-v1".to_string(),
                metadata_hash: "mhash".to_string(),
                external_etag: None,
                external_last_modified: None,
            });
        }

        /// The most important test in this module alongside the next
        /// one: an ingestor that could not observe its source must
        /// suppress the liveness sweep before it ever queries the store
        /// — an `UnreachableFetcher` alone would not distinguish "the
        /// sweep ran and found nothing" from "the sweep never ran",
        /// which is exactly what `LivenessStore::list_call_count`
        /// exists to tell apart.
        #[tokio::test]
        async fn incomplete_enumeration_guard_suppresses_the_sweep() {
            let source = make_feed_source("store-1");
            let config = make_ingestion_config("store-1");
            let store = LivenessStore::new(vec![row(
                "r1",
                "https://a.example.com/",
                Some(&old_timestamp()),
            )]);
            let embedder = FakeEmbedder::new(4);
            let mut doc_index = DocumentIndex::new();
            seed_doc_index_owned_by(&mut doc_index, &source.id, "https://a.example.com/");

            let ingestor = FakeIngestor::incomplete("feed unreachable");
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(
                store.list_call_count(),
                0,
                "Enumeration::Incomplete must suppress the liveness sweep before it \
                 ever queries the store"
            );
        }

        /// The steady-state feed-304 case: zero entry callbacks fire, so
        /// `seen` is empty. That suppresses the presumed-gone sweep for
        /// path/url sources, and deliberately does *not* suppress this
        /// one (specs/04-search-pipeline.md §1 "Guards").
        ///
        /// Suppressing here starved the mechanism in exactly the case it
        /// exists for: once a feed goes quiet its document stops changing,
        /// every run 304s, and the aged-out backlog is never probed again
        /// — this sweep being the only thing that could shrink it. Absence
        /// here only decides who gets probed; the delete still needs a
        /// confirmed 404/410.
        #[tokio::test]
        async fn zero_seen_on_a_feed_304_still_runs_the_sweep() {
            let source = make_feed_source("store-1");
            let config = make_ingestion_config("store-1");
            let store = LivenessStore::new(vec![row(
                "r1",
                "https://a.example.com/",
                Some(&old_timestamp()),
            )]);
            let embedder = FakeEmbedder::new(4);
            let mut doc_index = DocumentIndex::new();
            seed_doc_index_owned_by(&mut doc_index, &source.id, "https://a.example.com/");

            // Complete enumeration (the default), but zero callbacks —
            // an empty script, mirroring what `FeedIngestor` does on a
            // bare feed-document 304.
            let ingestor = FakeIngestor::new(vec![]);
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &ScriptedFetcher::new(ScriptedFetchOutcome::Gone),
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(
                store.list_call_count(),
                1,
                "an empty seen-set must not stop the sweep from querying for candidates"
            );
            assert_eq!(
                result.feed_entries_liveness_checked, 1,
                "the aged-out candidate must actually be probed"
            );
            assert_eq!(
                *store.delete_calls.lock().await,
                vec!["r1".to_string()],
                "and a confirmed 410 must still delete — the seen-set was never what \
                 licensed the delete"
            );
        }

        /// The empty seen-set subtracts nothing, which is the right answer
        /// for a 304'd run: the window is unchanged, so nothing aged out
        /// *during* the run and every candidate the query returns had
        /// already aged out before it began. The batch cap is what bounds
        /// the work, not the seen-set.
        #[tokio::test]
        async fn zero_seen_on_a_feed_304_is_still_bounded_by_the_batch_cap() {
            let source = make_feed_source("store-1");
            let config = make_ingestion_config("store-1");
            let rows: Vec<LivenessRow> = (0..40)
                .map(|i| {
                    row(
                        &format!("r{i}"),
                        &format!("https://a.example.com/{i}"),
                        Some(&old_timestamp()),
                    )
                })
                .collect();
            let store = LivenessStore::new(rows);
            let embedder = FakeEmbedder::new(4);
            let mut doc_index = DocumentIndex::new();
            for i in 0..40 {
                seed_doc_index_owned_by(
                    &mut doc_index,
                    &source.id,
                    &format!("https://a.example.com/{i}"),
                );
            }

            let ingestor = FakeIngestor::new(vec![]);
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &ScriptedFetcher::new(ScriptedFetchOutcome::NotModified),
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(
                result.feed_entries_liveness_checked, FEED_LIVENESS_BATCH_LIMIT as u64,
                "a zero-seen run probes at most one batch, same as any other run"
            );
        }

        /// The anomalous case must keep warning even on the feed branch:
        /// an ingestor that could not observe its source at all is never
        /// routine, regardless of source kind.
        #[tokio::test]
        async fn incomplete_enumeration_guard_on_a_feed_still_logs_at_warn() {
            let source = make_feed_source("store-1");
            let config = make_ingestion_config("store-1");
            let store = LivenessStore::new(vec![row(
                "r1",
                "https://a.example.com/",
                Some(&old_timestamp()),
            )]);
            let embedder = FakeEmbedder::new(4);
            let mut doc_index = DocumentIndex::new();
            seed_doc_index_owned_by(&mut doc_index, &source.id, "https://a.example.com/");

            let ingestor = FakeIngestor::incomplete("feed unreachable");
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };

            let (result, captured) = run_capturing_logs(&source, &ingestor, deps).await;
            result.unwrap();
            assert!(
                captured.contains("WARN") && captured.contains("skipping feed liveness sweep"),
                "an incomplete enumeration is genuinely anomalous and must still warn; \
                 captured: {captured}"
            );
        }

        #[tokio::test]
        async fn deletion_retain_performs_zero_liveness_fetches() {
            let source = make_feed_source("store-1");
            let config = make_ingestion_config("store-1");
            let store = LivenessStore::new(vec![row(
                "r1",
                "https://a.example.com/",
                Some(&old_timestamp()),
            )]);
            let embedder = FakeEmbedder::new(4);
            let mut doc_index = DocumentIndex::new();
            seed_doc_index_owned_by(&mut doc_index, &source.id, "https://a.example.com/");

            // Both guards pass this time (this run observes the owned
            // URI via a Skipped callback) — proving `Retain` alone, not
            // a guard, is what keeps this at zero fetches.
            let ingestor = FakeIngestor::new(vec![ScriptStep::Skipped(
                "https://a.example.com/".to_string(),
                SkipReason::Unchanged,
            )]);
            let deps = SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config);
            run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(
                store.list_call_count(),
                0,
                "DeletionPolicy::Retain must never reach the liveness sweep at all — \
                 there is no free preview signal for this mechanism"
            );
        }
    }
}
