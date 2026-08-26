//! `tool_get_document` tests: JSON shape, block-based text reconstruction,
//! store-scope visibility (E3), empty-id validation, and the `store`
//! discriminator (#144).

use std::sync::Arc;

use serde_json::Value;

use localdb_core::ids::{chunk_id, content_hash, new_ulid, resource_id};
use localdb_core::metadata::{DocumentMetadata, DublinCoreMetadata, Metadata};
use localdb_core::store::{FakeStore, RetrievalStore};
use localdb_core::{ChunkRecord, Span};

use crate::args::GetDocumentArgs;
use crate::tools::{tool_get_document, AvailableStore, StoreDescriptor};

use super::common::{backend_for, duplicate_doc_stores, make_chunk, make_descriptor, text_of};

#[tokio::test]
async fn tool_get_document_returns_identical_json_for_fixed_document() {
    let store_id = new_ulid();
    let origin_store = new_ulid();
    let source_id = new_ulid();
    let doc_uri = "file:///docs/guide.md";
    let doc_hash = content_hash("guide body");
    let doc_id = resource_id(doc_uri, &doc_hash);
    let metadata = Metadata::Document(DocumentMetadata {
        dublin_core: DublinCoreMetadata {
            title: Some("Guide".to_string()),
            creator: vec!["Ada".to_string()],
            subject: vec!["docs".to_string()],
            description: Some("reference document".to_string()),
            publisher: Some("localdb".to_string()),
            contributor: vec!["Bea".to_string()],
            date: Some("2026-06-29".to_string()),
            format: Some("text/markdown".to_string()),
            identifier: Some("guide-1".to_string()),
            language: Some("en".to_string()),
            rights: Some("CC0".to_string()),
            ..Default::default()
        },
        ..Default::default()
    });

    let store = FakeStore::new();
    let make_chunk = |text: &str| {
        let span = Span::new(0, text.len());
        ChunkRecord {
            id: chunk_id(&doc_id, 0, text, 0),
            resource_id: doc_id.clone(),
            store_id: store_id.clone(),
            text: text.to_string(),
            span,
            heading_path: vec!["Guide".to_string()],
            embedding: vec![0.1, 0.2],
            policy_version: "policy-v1".to_string(),
            fetched_at: "2026-06-29T00:00:00Z".to_string(),
            modified_at: Some("2026-06-29T00:00:00Z".to_string()),
            content_hash: doc_hash.clone(),
            origin_store: origin_store.clone(),
            source_id: source_id.clone(),
            ingestor_kind: "path".to_string(),
            mime: None,
            uri: doc_uri.to_string(),
            metadata: metadata.clone(),
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
    };
    store
        .upsert_chunks(vec![make_chunk("alpha"), make_chunk("beta")])
        .await
        .unwrap();

    let stores = vec![AvailableStore::from_arc(
        StoreDescriptor {
            id: store_id.to_string(),
            name: "notes".to_string(),
            visibility: "private".to_string(),
        },
        Arc::new(store),
    )];
    let args = GetDocumentArgs {
        id: doc_id.clone(),
        uri: None,
        store: None,
    };

    let backend = backend_for(&stores);
    let result = tool_get_document(&stores, backend.as_ref(), args).await;
    assert_ne!(result.is_error, Some(true));
    assert_eq!(result.content.len(), 1);

    let rendered_text = text_of(&result);

    let expected = serde_json::json!({
        "resource_id": doc_id,
        "uri": doc_uri,
        "title": "Guide",
        "date_original": null,
        "date_parsed": null,
        "store": {
            "id": store_id.to_string(),
            "name": "notes",
        },
        "provenance": {
            "fetched_at": "2026-06-29T00:00:00Z",
            "content_hash": doc_hash,
            "index_updated_at": null,
        },
        "metadata": metadata,
        "chunk_count": 2,
        "text": "alpha\nbeta",
    });
    let expected = serde_json::to_string_pretty(&expected).unwrap();

    assert_eq!(rendered_text, expected);
}

/// Regression test: `get_document` must reconstruct a multi-chunk table
/// from its persisted `blocks`, not by joining `ChunkRecord.text`. The
/// table chunker (spec 04 §3, intentional) re-emits the header +
/// `|---|` separator row in every chunk of a table split across
/// multiple chunks — joining chunk texts would duplicate that header
/// once per chunk. The single `Table` block holds the canonical text
/// with the header exactly once, so reconstruction from blocks must not
/// duplicate it.
#[tokio::test]
async fn tool_get_document_reconstructs_table_without_duplicated_header() {
    use localdb_core::block::{Block, BlockKind};
    use localdb_core::{chunk_blocks, CharSizer, ChunkerConfig};

    let store_id = new_ulid();
    let doc_uri = "file:///docs/table.md";

    // Same fixture shape as chunker.rs's own
    // `table_multi_chunk_split_preserves_header` unit test: with
    // target_tokens=40 and CharSizer, 2 data rows pack per chunk, so 10
    // rows split into 5 chunks, each re-emitting the header/separator.
    let table_text = {
        let mut md = String::from("| A | B |\n|---|---|\n");
        let rows: Vec<String> = (0..10).map(|i| format!("| {i} | {i} |")).collect();
        md.push_str(&rows.join("\n"));
        md
    };
    let doc_hash = content_hash(&table_text);
    let doc_id = resource_id(doc_uri, &doc_hash);

    let block = Block {
        seq: 0,
        kind: BlockKind::Table {
            headers: vec!["A".to_string(), "B".to_string()],
            rows: 10,
        },
        text: table_text.clone(),
        location: None,
    };

    let cfg = ChunkerConfig {
        preset: "prose".to_string(),
        target_tokens: Some(40),
        overlap_tokens: Some(0),
        window_turns: None,
        stride_turns: None,
    };
    let chunk_outputs = chunk_blocks(&doc_id, std::slice::from_ref(&block), &cfg, &CharSizer)
        .expect("chunking the table fixture must succeed");
    assert!(
        chunk_outputs.len() >= 2,
        "fixture must produce a multi-chunk table split, got {} chunk(s)",
        chunk_outputs.len()
    );

    let metadata = Metadata::default();
    let store = FakeStore::new();
    let chunk_records: Vec<ChunkRecord> = chunk_outputs
        .iter()
        .map(|co| ChunkRecord {
            id: co.id.clone(),
            resource_id: doc_id.clone(),
            store_id: store_id.clone(),
            text: co.text.clone(),
            span: co.span.clone(),
            heading_path: co.heading_path.clone(),
            embedding: vec![0.1, 0.2],
            policy_version: "policy-v1".to_string(),
            fetched_at: "2026-06-29T00:00:00Z".to_string(),
            modified_at: Some("2026-06-29T00:00:00Z".to_string()),
            content_hash: doc_hash.clone(),
            origin_store: store_id.clone(),
            source_id: "src-1".to_string(),
            ingestor_kind: "path".to_string(),
            mime: Some("text/markdown".to_string()),
            uri: doc_uri.to_string(),
            metadata: metadata.clone(),
            block_seq: co.block_seq,
            seq_in_block: co.seq_in_block,
            block_kind: co.block_kind.clone(),
            page: None,
            window_block_seqs: co.window_block_seqs.clone(),
            date_original: None,
            date_parsed: None,
            external_id: None,
            external_etag: None,
        })
        .collect();

    store
        .upsert_chunks_and_blocks(&store_id, &doc_id, chunk_records, &[block], None)
        .await
        .unwrap();

    let stores = vec![AvailableStore::from_arc(
        StoreDescriptor {
            id: store_id.to_string(),
            name: "notes".to_string(),
            visibility: "private".to_string(),
        },
        Arc::new(store),
    )];
    let args = GetDocumentArgs {
        id: doc_id.clone(),
        uri: None,
        store: None,
    };

    let backend = backend_for(&stores);
    let result = tool_get_document(&stores, backend.as_ref(), args).await;
    assert_ne!(result.is_error, Some(true));
    let rendered_text = text_of(&result);
    let parsed: Value = serde_json::from_str(&rendered_text).unwrap();
    let reconstructed = parsed["text"].as_str().unwrap();

    assert_eq!(
        reconstructed.matches("| A | B |").count(),
        1,
        "reconstructed text must contain the table header exactly once, \
         not once per chunk; got: {reconstructed:?}"
    );
    assert_eq!(
        reconstructed.matches("|---|---|").count(),
        1,
        "reconstructed text must contain the separator row exactly once; \
         got: {reconstructed:?}"
    );
    assert_eq!(
        reconstructed, table_text,
        "block-based reconstruction should equal the canonical block text exactly"
    );
}

// -----------------------------------------------------------------------
// E3 — get_document checks store scope visibility
// -----------------------------------------------------------------------

#[tokio::test]
async fn get_document_returns_not_found_when_store_id_mismatches() {
    // Set up a store whose descriptor id is "store-A" but the chunk's store_id
    // is "store-B" (simulating a federated/mismatched scenario).
    let fake = FakeStore::new();
    // Insert a chunk that claims to belong to "store-B", not "store-A".
    let chunk = make_chunk("chunk-1", "doc-mismatched", "store-B", "some content");
    fake.upsert_chunks(vec![chunk]).await.unwrap();

    // The AvailableStore has descriptor id "store-A" — the chunk's store_id doesn't match.
    let av = AvailableStore::new(make_descriptor("store-A", "store-a"), Box::new(fake));

    let args = GetDocumentArgs {
        id: "doc-mismatched".to_string(),
        uri: None,
        store: None,
    };
    let stores = [av];
    let backend = backend_for(&stores);
    let result = tool_get_document(&stores, backend.as_ref(), args).await;

    // The tool should hide the document (not leak existence) and return not-found.
    assert_eq!(
        result.is_error,
        Some(true),
        "mismatched store_id should cause resource_not_found"
    );
    let text = text_of(&result);
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("error body is JSON");
    assert_eq!(
        parsed["error"]["code"].as_str().unwrap(),
        "resource_not_found",
    );
}

#[tokio::test]
async fn get_document_succeeds_when_store_id_matches() {
    let fake = FakeStore::new();
    let chunk = make_chunk("chunk-1", "doc-1", "store-A", "hello world");
    fake.upsert_chunks(vec![chunk]).await.unwrap();

    let av = AvailableStore::new(make_descriptor("store-A", "store-a"), Box::new(fake));

    let args = GetDocumentArgs {
        id: "doc-1".to_string(),
        uri: None,
        store: None,
    };
    let stores = [av];
    let backend = backend_for(&stores);
    let result = tool_get_document(&stores, backend.as_ref(), args).await;

    assert_ne!(
        result.is_error,
        Some(true),
        "matching store_id should succeed"
    );
    let text = text_of(&result);
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("success body is JSON");
    assert_eq!(parsed["resource_id"].as_str().unwrap(), "doc-1");
    assert!(
        parsed.get("metadata").is_some(),
        "metadata field must be present"
    );
}

#[tokio::test]
async fn get_document_metadata_carries_through() {
    let fake = FakeStore::new();
    let mut chunk = make_chunk("chunk-1", "doc-meta", "store-A", "text content");
    chunk.metadata =
        localdb_core::metadata::Metadata::Document(localdb_core::metadata::DocumentMetadata {
            dublin_core: localdb_core::metadata::DublinCoreMetadata {
                title: Some("Rich Doc".to_string()),
                creator: vec!["Carol".to_string()],
                date: Some("2026-05-01".to_string()),
                ..Default::default()
            },
            ..Default::default()
        });
    fake.upsert_chunks(vec![chunk]).await.unwrap();

    let av = AvailableStore::new(make_descriptor("store-A", "store-a"), Box::new(fake));

    let args = GetDocumentArgs {
        id: "doc-meta".to_string(),
        uri: None,
        store: None,
    };
    let stores = [av];
    let backend = backend_for(&stores);
    let result = tool_get_document(&stores, backend.as_ref(), args).await;

    assert_ne!(result.is_error, Some(true));
    let text = text_of(&result);
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    let meta = &parsed["metadata"];
    assert_eq!(meta["title"].as_str().unwrap(), "Rich Doc");
    assert_eq!(
        meta["creator"].as_array().unwrap()[0].as_str().unwrap(),
        "Carol"
    );
    assert_eq!(meta["date"].as_str().unwrap(), "2026-05-01");
}

#[tokio::test]
async fn get_document_empty_id_returns_typed_error() {
    let fake = FakeStore::new();
    let av = AvailableStore::new(make_descriptor("store-1", "s1"), Box::new(fake));

    // Empty 'id', no 'uri' either. `id` is `#[serde(default)]` (see
    // args.rs), not schema-required, so an omitted `id` reaches this
    // same tool-level "must not be empty" path rather than failing at
    // deserialization — this exercises that path directly.
    let args = GetDocumentArgs {
        id: String::new(),
        uri: None,
        store: None,
    };
    let stores = [av];
    let backend = backend_for(&stores);
    let result = tool_get_document(&stores, backend.as_ref(), args).await;
    assert_eq!(result.is_error, Some(true));
    let text = text_of(&result);
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("must be JSON");
    assert_eq!(parsed["error"]["code"].as_str().unwrap(), "invalid_request");
}

#[tokio::test]
async fn get_document_empty_id_with_uri_mentions_search_result() {
    let fake = FakeStore::new();
    let av = AvailableStore::new(make_descriptor("store-1", "s1"), Box::new(fake));

    let args = GetDocumentArgs {
        id: String::new(),
        uri: Some("file:///docs/guide.md".to_string()),
        store: None,
    };
    let stores = [av];
    let backend = backend_for(&stores);
    let result = tool_get_document(&stores, backend.as_ref(), args).await;
    assert_eq!(result.is_error, Some(true));
    let text = text_of(&result);
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("must be JSON");
    assert_eq!(parsed["error"]["code"].as_str().unwrap(), "invalid_request");
    assert!(
        parsed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not supported in v1"),
        "message should point the caller at 'id' from a search result"
    );
}

// -----------------------------------------------------------------------
// #144 — `store` discriminator on get_document / get_chunks
// -----------------------------------------------------------------------

#[tokio::test]
async fn get_document_with_store_name_disambiguates_duplicate_id_across_stores() {
    let (av_a, av_b) = duplicate_doc_stores("dup-doc").await;
    let stores = vec![av_a, av_b];
    let backend = backend_for(&stores);

    let mut args_a = GetDocumentArgs {
        id: "dup-doc".to_string(),
        uri: None,
        store: None,
    };
    args_a.store = Some("store-a".to_string());
    let result_a = tool_get_document(&stores, backend.as_ref(), args_a).await;
    assert_ne!(result_a.is_error, Some(true));
    let parsed_a: serde_json::Value = serde_json::from_str(&text_of(&result_a)).unwrap();
    assert_eq!(parsed_a["text"].as_str().unwrap(), "from store A");
    assert_eq!(parsed_a["store"]["name"].as_str().unwrap(), "store-a");

    let args_b = GetDocumentArgs {
        id: "dup-doc".to_string(),
        uri: None,
        store: Some("store-b".to_string()),
    };
    let result_b = tool_get_document(&stores, backend.as_ref(), args_b).await;
    assert_ne!(result_b.is_error, Some(true));
    let parsed_b: serde_json::Value = serde_json::from_str(&text_of(&result_b)).unwrap();
    assert_eq!(parsed_b["text"].as_str().unwrap(), "from store B");
    assert_eq!(parsed_b["store"]["name"].as_str().unwrap(), "store-b");
}

#[tokio::test]
async fn get_document_with_store_id_also_disambiguates() {
    let (av_a, av_b) = duplicate_doc_stores("dup-doc").await;
    let stores = vec![av_a, av_b];
    let backend = backend_for(&stores);

    let args = GetDocumentArgs {
        id: "dup-doc".to_string(),
        uri: None,
        store: Some("store-B-id".to_string()),
    };
    let result = tool_get_document(&stores, backend.as_ref(), args).await;
    assert_ne!(result.is_error, Some(true));
    let parsed: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
    assert_eq!(parsed["text"].as_str().unwrap(), "from store B");
    assert_eq!(parsed["store"]["id"].as_str().unwrap(), "store-B-id");
}

#[tokio::test]
async fn get_document_unknown_store_returns_store_not_found() {
    let (av_a, av_b) = duplicate_doc_stores("dup-doc").await;
    let stores = vec![av_a, av_b];
    let backend = backend_for(&stores);

    let args = GetDocumentArgs {
        id: "dup-doc".to_string(),
        uri: None,
        store: Some("no-such-store".to_string()),
    };
    let result = tool_get_document(&stores, backend.as_ref(), args).await;
    assert_eq!(result.is_error, Some(true));
    let parsed: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
    assert_eq!(parsed["error"]["code"].as_str().unwrap(), "store_not_found");
}

#[tokio::test]
async fn get_document_omitted_store_keeps_first_match_backward_compat() {
    let (av_a, av_b) = duplicate_doc_stores("dup-doc").await;
    let stores = vec![av_a, av_b];
    let backend = backend_for(&stores);

    let args = GetDocumentArgs {
        id: "dup-doc".to_string(),
        uri: None,
        store: None,
    };
    let result = tool_get_document(&stores, backend.as_ref(), args).await;
    assert_ne!(result.is_error, Some(true));
    let parsed: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
    assert_eq!(
        parsed["text"].as_str().unwrap(),
        "from store A",
        "omitted store must keep pre-#144 first-match-wins behavior"
    );
}

// -----------------------------------------------------------------------
// `StoresBackend::find_document` — trait contract for an unscoped lookup
// -----------------------------------------------------------------------

#[tokio::test]
async fn stores_backend_find_document_unscoped_rejects_cross_store_ambiguity() {
    let (av_a, av_b) = duplicate_doc_stores("dup-doc").await;
    let stores = vec![av_a, av_b];
    let backend = backend_for(&stores);

    let err = backend
        .find_document("dup-doc", None)
        .await
        .expect_err("a document present in two stores must be ambiguous when unscoped");
    match err {
        localdb_core::Error::InvalidRequest { message } => {
            assert!(
                message.contains("dup-doc") && message.contains("multiple stores"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected Error::InvalidRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn stores_backend_find_document_unscoped_returns_unique_match() {
    let (av_a, av_b) = duplicate_doc_stores("dup-doc").await;
    // Only store A actually holds "solo-doc".
    let stores = vec![av_a, av_b];
    let backend = backend_for(&stores);

    let info = backend
        .find_document("solo-doc-does-not-exist", None)
        .await
        .unwrap();
    assert!(info.is_none());

    let info = backend.find_document("dup-doc", Some("store-A-id")).await;
    assert!(
        info.is_ok(),
        "scoped lookup must still succeed unambiguously"
    );
}
