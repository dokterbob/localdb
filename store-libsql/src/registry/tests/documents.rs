use super::*;

// ---------------------------------------------------------------------------
// Documents (list_documents / store-scoped find_document)
// ---------------------------------------------------------------------------

fn make_document_chunk(
    chunk_id: &str,
    resource_id: &str,
    store_id: &str,
    source_id: &str,
    uri: &str,
) -> ChunkRecord {
    ChunkRecord {
        id: chunk_id.to_string(),
        resource_id: resource_id.to_string(),
        store_id: store_id.to_string(),
        text: "some chunk text".to_string(),
        span: Span::new(0, 15),
        heading_path: vec![],
        embedding: vec![0.1, 0.2, 0.3, 0.4],
        policy_version: "v1".to_string(),
        fetched_at: "2026-07-01T00:00:00Z".to_string(),
        modified_at: Some("2026-07-01T00:00:00Z".to_string()),
        content_hash: "abc123".to_string(),
        origin_store: store_id.to_string(),
        source_id: source_id.to_string(),
        ingestor_kind: "path".to_string(),
        mime: Some("text/markdown".to_string()),
        uri: uri.to_string(),
        metadata: Metadata::default(),
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
async fn list_documents_filters_by_store() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-1", "notes"))
        .await
        .unwrap();
    api.upsert_store(&make_store("store-2", "other"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-1", "store-1", "/a"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-2", "store-2", "/b"))
        .await
        .unwrap();

    let handle1 = api.retrieval_store("store-1").await.unwrap();
    handle1
        .upsert_chunks(vec![make_document_chunk(
            "c1",
            "doc-1",
            "store-1",
            "src-1",
            "file:///a/1.md",
        )])
        .await
        .unwrap();
    let handle2 = api.retrieval_store("store-2").await.unwrap();
    handle2
        .upsert_chunks(vec![make_document_chunk(
            "c2",
            "doc-2",
            "store-2",
            "src-2",
            "file:///b/1.md",
        )])
        .await
        .unwrap();

    let docs = api.list_documents("store-1", None, None, 0).await.unwrap();
    assert_eq!(docs.len(), 1, "must only return store-1's document");
    assert_eq!(docs[0].id, "doc-1");
}

#[tokio::test]
async fn list_documents_filters_by_source() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-1", "notes"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-1", "store-1", "/a"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-2", "store-1", "/b"))
        .await
        .unwrap();

    let handle = api.retrieval_store("store-1").await.unwrap();
    handle
        .upsert_chunks(vec![
            make_document_chunk("c1", "doc-1", "store-1", "src-1", "file:///a/1.md"),
            make_document_chunk("c2", "doc-2", "store-1", "src-2", "file:///b/1.md"),
        ])
        .await
        .unwrap();

    let docs = api
        .list_documents("store-1", Some("src-1"), None, 0)
        .await
        .unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].id, "doc-1");
}

#[tokio::test]
async fn list_documents_cross_store_source_id_yields_empty() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-1", "notes"))
        .await
        .unwrap();
    api.upsert_store(&make_store("store-2", "other"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-1", "store-1", "/a"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-2", "store-2", "/b"))
        .await
        .unwrap();

    let handle1 = api.retrieval_store("store-1").await.unwrap();
    handle1
        .upsert_chunks(vec![make_document_chunk(
            "c1",
            "doc-1",
            "store-1",
            "src-1",
            "file:///a/1.md",
        )])
        .await
        .unwrap();

    // src-2 belongs to store-2, not store-1: filtering store-1 by src-2 is an
    // unknown-source-id-for-this-store filter, which yields an empty list
    // rather than an error or leaking store-2's data.
    let docs = api
        .list_documents("store-1", Some("src-2"), None, 0)
        .await
        .unwrap();
    assert!(docs.is_empty());
}

#[tokio::test]
async fn list_documents_orders_by_uri() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-1", "notes"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-1", "store-1", "/a"))
        .await
        .unwrap();

    let handle = api.retrieval_store("store-1").await.unwrap();
    handle
        .upsert_chunks(vec![
            make_document_chunk("c1", "doc-z", "store-1", "src-1", "file:///z.md"),
            make_document_chunk("c2", "doc-a", "store-1", "src-1", "file:///a.md"),
            make_document_chunk("c3", "doc-m", "store-1", "src-1", "file:///m.md"),
        ])
        .await
        .unwrap();

    let docs = api.list_documents("store-1", None, None, 0).await.unwrap();
    let uris: Vec<&str> = docs.iter().map(|d| d.uri.as_str()).collect();
    assert_eq!(
        uris,
        vec!["file:///a.md", "file:///m.md", "file:///z.md"],
        "documents must come back ordered by uri regardless of insertion order"
    );
}

async fn seed_five_documents(api: &SqliteBackend) {
    api.upsert_store(&make_store("store-1", "notes"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-1", "store-1", "/a"))
        .await
        .unwrap();

    let handle = api.retrieval_store("store-1").await.unwrap();
    let chunks: Vec<ChunkRecord> = (0..5)
        .map(|i| {
            make_document_chunk(
                &format!("c{i}"),
                &format!("doc-{i}"),
                "store-1",
                "src-1",
                &format!("file:///{i}.md"),
            )
        })
        .collect();
    handle.upsert_chunks(chunks).await.unwrap();
}

#[tokio::test]
async fn list_documents_limit_and_offset_page_through_the_result_set() {
    let (_dir, api) = make_api().await;
    seed_five_documents(&api).await;

    let page1 = api
        .list_documents("store-1", None, Some(2), 0)
        .await
        .unwrap();
    let uris1: Vec<&str> = page1.iter().map(|d| d.uri.as_str()).collect();
    assert_eq!(uris1, vec!["file:///0.md", "file:///1.md"]);

    let page2 = api
        .list_documents("store-1", None, Some(2), 2)
        .await
        .unwrap();
    let uris2: Vec<&str> = page2.iter().map(|d| d.uri.as_str()).collect();
    assert_eq!(uris2, vec!["file:///2.md", "file:///3.md"]);

    let page3 = api
        .list_documents("store-1", None, Some(2), 4)
        .await
        .unwrap();
    let uris3: Vec<&str> = page3.iter().map(|d| d.uri.as_str()).collect();
    assert_eq!(uris3, vec!["file:///4.md"]);
}

#[tokio::test]
async fn list_documents_offset_beyond_end_yields_empty() {
    let (_dir, api) = make_api().await;
    seed_five_documents(&api).await;

    let docs = api
        .list_documents("store-1", None, Some(10), 100)
        .await
        .unwrap();
    assert!(docs.is_empty());
}

#[tokio::test]
async fn list_documents_none_limit_returns_every_row_from_offset_onward() {
    let (_dir, api) = make_api().await;
    seed_five_documents(&api).await;

    let docs = api.list_documents("store-1", None, None, 2).await.unwrap();
    let uris: Vec<&str> = docs.iter().map(|d| d.uri.as_str()).collect();
    assert_eq!(
        uris,
        vec!["file:///2.md", "file:///3.md", "file:///4.md"],
        "limit=None must return every remaining row uncapped, honoring offset"
    );
}

#[tokio::test]
async fn count_documents_matches_total_row_count() {
    let (_dir, api) = make_api().await;
    seed_five_documents(&api).await;

    let count = api.count_documents("store-1", None).await.unwrap();
    assert_eq!(count, 5);
}

#[tokio::test]
async fn count_documents_respects_source_filter() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-1", "notes"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-1", "store-1", "/a"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-2", "store-1", "/b"))
        .await
        .unwrap();

    let handle = api.retrieval_store("store-1").await.unwrap();
    handle
        .upsert_chunks(vec![
            make_document_chunk("c1", "doc-1", "store-1", "src-1", "file:///a/1.md"),
            make_document_chunk("c2", "doc-2", "store-1", "src-2", "file:///b/1.md"),
            make_document_chunk("c3", "doc-3", "store-1", "src-2", "file:///b/2.md"),
        ])
        .await
        .unwrap();

    assert_eq!(
        api.count_documents("store-1", Some("src-1")).await.unwrap(),
        1
    );
    assert_eq!(
        api.count_documents("store-1", Some("src-2")).await.unwrap(),
        2
    );
    assert_eq!(api.count_documents("store-1", None).await.unwrap(), 3);
}

/// Mirrors `find_document_errors_when_id_exists_in_multiple_stores`
/// (`store-libsql/tests/conformance.rs`): the same document id lives in two
/// stores. A store-scoped lookup resolves to exactly that store's row with
/// no ambiguity; the unscoped lookup still errors.
#[tokio::test]
async fn find_document_with_store_id_disambiguates_cross_store_id() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-A", "store-A"))
        .await
        .unwrap();
    api.upsert_store(&make_store("store-B", "store-B"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-A", "store-A", "/a"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-B", "store-B", "/b"))
        .await
        .unwrap();

    let handle_a = api.retrieval_store("store-A").await.unwrap();
    handle_a
        .upsert_chunks(vec![make_document_chunk(
            "chunk-a",
            "doc-shared",
            "store-A",
            "src-A",
            "file:///a/doc.md",
        )])
        .await
        .unwrap();
    let handle_b = api.retrieval_store("store-B").await.unwrap();
    handle_b
        .upsert_chunks(vec![make_document_chunk(
            "chunk-b",
            "doc-shared",
            "store-B",
            "src-B",
            "file:///b/doc.md",
        )])
        .await
        .unwrap();

    let found_a = api
        .find_document("doc-shared", Some("store-A"))
        .await
        .unwrap()
        .expect("doc-shared must be found scoped to store-A");
    assert_eq!(found_a.store_id, "store-A");
    assert_eq!(found_a.uri, "file:///a/doc.md");

    let found_b = api
        .find_document("doc-shared", Some("store-B"))
        .await
        .unwrap()
        .expect("doc-shared must be found scoped to store-B");
    assert_eq!(found_b.store_id, "store-B");
    assert_eq!(found_b.uri, "file:///b/doc.md");

    let unscoped = api.find_document("doc-shared", None).await;
    assert!(
        matches!(unscoped, Err(Error::InvalidRequest { .. })),
        "the unscoped lookup must still error as ambiguous; got: {:?}",
        unscoped
    );
}

/// `find_document` must read `resources.date_original`/`date_parsed` (stamped
/// via `ChunkRecord::date_original`/`date_parsed` on upsert) and
/// `index_updated_at` (the write-time clock stamped by every
/// `upsert_chunks_inner` call, migration v7) back onto `DocumentInfo`. All
/// three are `Option` — a resource with no claimed date at all still finds
/// successfully with `date_original`/`date_parsed: None`.
#[tokio::test]
async fn find_document_returns_dates_and_index_updated_at() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-1", "notes"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-1", "store-1", "/a"))
        .await
        .unwrap();

    let handle = api.retrieval_store("store-1").await.unwrap();
    let mut chunk = make_document_chunk("c1", "doc-1", "store-1", "src-1", "file:///a/1.md");
    chunk.date_original = Some("2026-06-15T10:30:00Z".to_string());
    chunk.date_parsed = Some("2026-06-15".to_string());
    handle.upsert_chunks(vec![chunk]).await.unwrap();

    let found = api
        .find_document("doc-1", Some("store-1"))
        .await
        .unwrap()
        .expect("doc-1 must be found");
    assert_eq!(found.date_original.as_deref(), Some("2026-06-15T10:30:00Z"));
    assert_eq!(found.date_parsed.as_deref(), Some("2026-06-15"));
    assert!(
        found.index_updated_at.is_some(),
        "index_updated_at must be populated (stamped by every upsert)"
    );
}

/// A resource with no claimed date at all (`ChunkRecord::date_original`/
/// `date_parsed` left `None`, the default) reads back as `None`, not an
/// error or an empty string.
#[tokio::test]
async fn find_document_dates_are_none_when_never_stamped() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-1", "notes"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-1", "store-1", "/a"))
        .await
        .unwrap();

    let handle = api.retrieval_store("store-1").await.unwrap();
    handle
        .upsert_chunks(vec![make_document_chunk(
            "c1",
            "doc-1",
            "store-1",
            "src-1",
            "file:///a/1.md",
        )])
        .await
        .unwrap();

    let found = api
        .find_document("doc-1", Some("store-1"))
        .await
        .unwrap()
        .expect("doc-1 must be found");
    assert_eq!(found.date_original, None);
    assert_eq!(found.date_parsed, None);
    assert!(found.index_updated_at.is_some());
}
