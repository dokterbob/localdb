//! Cross-crate regression tests for the delete-sweep's "empty ≠ unavailable"
//! family — a real `FileIngestor` over real filesystem I/O, composed with
//! `core::ingestion::run_source_ingestion`.
//!
//! Three incidents, one conflation ("I observed nothing" read as "it was
//! deleted"), guarded here end to end:
//!
//!   - `transient_read_error_on_space_named_file_does_not_delete_it` — a file
//!     that exists but fails to read (the original `on_skipped` URI-mismatch
//!     bug).
//!   - `unreachable_root_does_not_delete_indexed_documents` — issue #156: an
//!     unmounted volume took the `books` store from 5,015 documents to 428.
//!   - `emptied_file_does_not_delete_previously_indexed_content` — issue #185:
//!     a file that extracts to nothing erasing its own indexed content.
//!
//! The first test's original header follows, kept because it explains why its
//! fixture filename contains a space.
//!
//! ---
//!
//! Cross-crate regression test for the `on_skipped` raw-vs-normalized URI
//! mismatch in the delete-sweep (see `core/src/ingestion.rs`'s
//! `PipelineCallback::seen` and `is_uri_from_source`'s "Normalization" doc
//! comment).
//!
//! `PipelineCallback::on_resource` marks a URI "seen" using
//! `resource.uri.as_str()` — a `Uri`, normalized by `url::Url::parse`
//! (percent-encoded path bytes, etc.). `PipelineCallback::on_skipped` instead
//! marks "seen" using the raw `&str` the ingestor passed in. `FileIngestor`
//! (in this crate) passes the *raw* `file.uri` string (built by
//! `core::ingestion::enumerate_dir` as `format!("file://{}", abs_path.display())`)
//! to `on_skipped` on every I/O-error path, while the success path instead
//! runs that same string through `Uri::parse` before handing it to
//! `on_resource`.
//!
//! A filename containing a space makes the two representations differ in
//! bytes (`file:///.../my file.md` vs. `file:///.../my%20file.md`), so a
//! *second* run in which the file transiently fails to read (`on_skipped`
//! with the raw URI) leaves the delete-sweep's `seen` set holding a key that
//! never matches the normalized key already in `DocumentIndex`/the store —
//! and the sweep deletes a document that is still alive on disk, for no
//! reason other than a transient permission/read error.
//!
//! This test only composes `ingest::FileIngestor` with
//! `core::ingestion::run_source_ingestion` (the two crates that must both be
//! involved to observe the bug); it makes no production changes.
//!
//! # What this still proves, and what it no longer can
//!
//! The description above is the bug as it existed before `on_skipped` took a
//! `&Uri`. It is kept because it explains *why* this test exists and why the
//! fixture filename contains a space. Be precise about its current value,
//! though: once `FileIngestor` shares one `file.uri: Uri` between
//! `on_resource` and `on_skipped`, the raw-vs-normalized divergence is
//! unrepresentable *through this ingestor*, so removing the space from the
//! fixture would no longer change the outcome.
//!
//! What it does still guard, end to end and with real filesystem I/O, is the
//! invariant that made the original bug destructive: **an ingestor must report
//! a failed read via `on_skipped`, and a resource so reported must survive the
//! delete-sweep.** Deleting the `on_skipped(&file.uri, SkipReason::Error(..))`
//! call in `file_ingestor.rs`'s read-error arm fails this test in both debug
//! (the `debug_assert_eq!` on `errors == skip_error_count`) and release (the
//! `error_count == 1` assertion below) — verified by mutation.

use std::os::unix::fs::PermissionsExt;

use localdb_core::embedder::FakeEmbedder;
use localdb_core::ids::new_ulid;
use localdb_core::ingestion::{
    run_source_ingestion, DeletionPolicy, DocumentIndex, DocumentRecord, IngestionConfig,
    SourceIngestionDeps,
};
use localdb_core::store::FakeStore;
use localdb_core::types::{Source, SourceKind, SourceSpec};
use localdb_core::{ChunkerConfig, RetrievalStore};

use ingest::FileIngestor;

fn make_source(root: &str) -> Source {
    Source {
        id: new_ulid(),
        store_id: new_ulid(),
        kind: SourceKind::Path,
        spec: SourceSpec::Path {
            root: root.to_string(),
            include: vec![],
            exclude: vec![],
        },
        source_preset: "prose".to_string(),
    }
}

/// A filename containing a space is the minimal repro: `url::Url::parse`
/// percent-encodes the space, so the raw filesystem-derived URI and the
/// `Uri`-normalized one differ in bytes, which is exactly what makes the
/// `on_resource`/`on_skipped` `seen`-set mismatch observable.
#[cfg(unix)]
#[tokio::test]
async fn transient_read_error_on_space_named_file_does_not_delete_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file_path = dir.path().join("my file.md");
    std::fs::write(
        &file_path,
        "# Title\n\nSome content for the regression test.",
    )
    .expect("write fixture file");

    let root = dir.path().to_str().expect("utf8 tempdir path").to_string();
    let source = make_source(&root);

    let store = FakeStore::new();
    let embedder = FakeEmbedder::new(4);
    let config = IngestionConfig {
        store_id: source.store_id.clone(),
        policy_version: "policy-v1".to_string(),
        chunker: ChunkerConfig::prose(),
    };

    let parser =
        extract::build_chain(&extract::default_parser_ids()).expect("build default parser chain");
    let ingestor = FileIngestor::new(Box::new(parser));

    let mut doc_index = DocumentIndex::new();

    // --- Run 1: clean index of the space-named file. ---
    {
        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
        };
        let result = run_source_ingestion(&source, &ingestor, deps)
            .await
            .expect("first run must not error");
        assert_eq!(
            result.docs_indexed, 1,
            "the space-named file should index cleanly on the first run"
        );
    }

    let uris = doc_index.uris();
    assert_eq!(
        uris.len(),
        1,
        "exactly one document should be tracked after the first run"
    );
    let resource_id = doc_index
        .get(&uris[0])
        .expect("just-inserted uri must be present")
        .resource_id
        .clone();

    let chunks_before = store
        .get_chunks_for_resource(&resource_id)
        .await
        .expect("get_chunks_for_resource must not error");
    assert!(
        !chunks_before.is_empty(),
        "the first run must have written chunks for the document"
    );

    // --- Force a read error on the second run via chmod 0. ---
    let original_perms = std::fs::metadata(&file_path)
        .expect("stat fixture file")
        .permissions();
    std::fs::set_permissions(&file_path, std::fs::Permissions::from_mode(0o000))
        .expect("chmod 0 the fixture file");

    // Guard against a root test runner: root ignores permission bits, so
    // `std::fs::read` would still succeed and the rest of this test would be
    // meaningless. Restore permissions and bail out early rather than adding
    // a `libc` dependency to check the effective uid directly.
    if std::fs::read(&file_path).is_ok() {
        std::fs::set_permissions(&file_path, original_perms).expect("restore permissions");
        eprintln!(
            "skipping transient_read_error_on_space_named_file_does_not_delete_it: \
             running as root, permission bits are ignored"
        );
        return;
    }

    // --- Run 2: the read fails -> FileIngestor reports on_skipped(Error) with
    // the RAW (un-normalized) uri; enumeration still discovers the file. ---
    let run2_result = {
        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
        };
        run_source_ingestion(&source, &ingestor, deps).await
    };

    // Restore permissions immediately after the run and before any
    // assert/unwrap below, so a failing assertion doesn't leave behind a
    // tempdir that `tempfile` cannot clean up on drop.
    std::fs::set_permissions(&file_path, original_perms).expect("restore permissions");

    let result = run2_result.expect("second run must not itself error");

    assert_eq!(
        result.error_count, 1,
        "the transient read failure must be reported as an error"
    );
    assert_eq!(
        result.docs_deleted, 0,
        "a transient read error must NOT delete the still-existing document — \
         this is the raw-vs-normalized URI mismatch between on_resource's \
         `resource.uri.as_str()` and on_skipped's raw `&file.uri` string \
         landing in the delete-sweep's `seen` set under different keys"
    );

    let chunks_after = store
        .get_chunks_for_resource(&resource_id)
        .await
        .expect("get_chunks_for_resource must not error");
    assert!(
        !chunks_after.is_empty(),
        "chunks for the document must survive a transient read error, but were deleted"
    );
}

/// Shared harness: index `root` once, run `mutate`, index again, and return
/// (the records the first run indexed, the second run's result, the store)
/// for assertions.
///
/// Both scenarios below have the same shape — index, perturb the filesystem,
/// re-index, assert nothing was deleted — and the interesting part is the
/// perturbation, not the plumbing.
async fn index_mutate_reindex(
    root: &std::path::Path,
    mutate: impl FnOnce(),
) -> (
    Vec<DocumentRecord>,
    localdb_core::ingestion::IngestionResult,
    FakeStore,
) {
    let source = make_source(root.to_str().expect("utf8 tempdir path"));
    let store = FakeStore::new();
    let embedder = FakeEmbedder::new(4);
    let config = IngestionConfig {
        store_id: source.store_id.clone(),
        policy_version: "policy-v1".to_string(),
        chunker: ChunkerConfig::prose(),
    };
    let parser =
        extract::build_chain(&extract::default_parser_ids()).expect("build default parser chain");
    let ingestor = FileIngestor::new(Box::new(parser));

    let mut doc_index = DocumentIndex::new();
    {
        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Prune,
        };
        let first = run_source_ingestion(&source, &ingestor, deps)
            .await
            .expect("first run must not error");
        assert!(
            first.docs_indexed > 0,
            "test setup invariant: the first run must index something"
        );
    }
    let after_first: Vec<DocumentRecord> = doc_index
        .uris()
        .iter()
        .map(|uri| doc_index.get(uri).expect("record for known uri").clone())
        .collect();

    mutate();

    let deps = SourceIngestionDeps {
        doc_index: &mut doc_index,
        store: &store,
        embedder: &embedder,
        config: &config,
        progress: None,
        deletion: DeletionPolicy::Prune,
    };
    let second = run_source_ingestion(&source, &ingestor, deps)
        .await
        .expect("second run must not error");

    (after_first, second, store)
}

/// **Issue #156, the reported incident.** An indexed source whose root becomes
/// unreachable — an unmounted volume, a detached external drive, a moved
/// directory — must not have its documents swept. The real-world failure took
/// the `books` store from 5,015 resources to 428 when `/Volumes/Archive` was
/// unmounted.
#[tokio::test]
async fn unreachable_root_does_not_delete_indexed_documents() {
    let parent = tempfile::tempdir().expect("tempdir");
    let root = parent.path().join("library");
    std::fs::create_dir(&root).expect("create root");
    for (name, body) in [
        ("a.md", "# Alpha\n\nFirst document body."),
        ("b.md", "# Bravo\n\nSecond document body."),
        ("c.md", "# Charlie\n\nThird document body."),
    ] {
        std::fs::write(root.join(name), body).expect("write fixture");
    }

    let root_for_mutate = root.clone();
    let (after_first, second, store) = index_mutate_reindex(&root, move || {
        // The volume goes away between runs.
        std::fs::remove_dir_all(&root_for_mutate).expect("remove the root");
    })
    .await;

    assert_eq!(after_first.len(), 3, "all three files indexed on run 1");
    assert_eq!(
        second.docs_deleted, 0,
        "an unreachable root must delete nothing: enumeration produced no \
         evidence about what still exists (#156)"
    );

    for record in &after_first {
        let chunks = store
            .get_chunks_for_resource(&record.resource_id)
            .await
            .expect("get_chunks_for_resource must not error");
        assert!(
            !chunks.is_empty(),
            "chunks for {} must survive an unreachable root",
            record.uri
        );
    }
}

/// **Issue #185.** A file that is still present but extracts to nothing must
/// not erase its own previously indexed content — and must not be reported as
/// a successfully indexed document either.
#[tokio::test]
async fn emptied_file_does_not_delete_previously_indexed_content() {
    let dir = tempfile::tempdir().expect("tempdir");
    let emptied = dir.path().join("emptied.md");
    std::fs::write(&emptied, "# Emptied\n\nContent that is about to vanish.").expect("write");
    std::fs::write(dir.path().join("stable.md"), "# Stable\n\nUntouched body.").expect("write");

    let emptied_for_mutate = emptied.clone();
    let (after_first, second, store) = index_mutate_reindex(dir.path(), move || {
        // Truncated in place: still enumerated, still readable, no content.
        std::fs::write(&emptied_for_mutate, "").expect("truncate the file");
    })
    .await;

    assert_eq!(after_first.len(), 2, "both files indexed on run 1");
    assert_eq!(
        second.docs_deleted, 0,
        "an emptied file must not delete its own indexed content (#185)"
    );
    assert_eq!(
        second.docs_indexed, 0,
        "nothing new was written: the emptied file produced no content and the \
         stable file was unchanged"
    );
    assert_eq!(
        second.docs_skipped, 2,
        "the emptied file is skipped as contentless, the stable one as unchanged"
    );

    // Canonicalize first: enumeration does, and on macOS a tempdir lives under
    // `/var/folders/...`, a symlink to `/private/var/folders/...`, so the
    // uncanonicalized path builds a URI that matches nothing.
    let emptied_uri = localdb_core::uri::Uri::from_file_path(
        &emptied
            .canonicalize()
            .expect("canonicalize the emptied file"),
    )
    .expect("absolute path")
    .as_str()
    .to_string();
    let record = after_first
        .iter()
        .find(|r| r.uri == emptied_uri)
        .expect("the emptied file must have been indexed on run 1");
    let chunks = store
        .get_chunks_for_resource(&record.resource_id)
        .await
        .expect("get_chunks_for_resource must not error");
    assert!(
        !chunks.is_empty(),
        "the emptied file's previously indexed content must still be searchable"
    );
}
