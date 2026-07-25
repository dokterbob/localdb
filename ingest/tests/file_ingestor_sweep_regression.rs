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

use std::os::unix::fs::PermissionsExt;

use localdb_core::embedder::FakeEmbedder;
use localdb_core::ids::new_ulid;
use localdb_core::ingestion::{
    run_source_ingestion, DocumentIndex, IngestionConfig, SourceIngestionDeps,
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
