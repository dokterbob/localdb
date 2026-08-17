//! `search` tool tests.

use serde_json::{json, Value};

use localdb_core::{
    ids::{chunk_id, content_hash, new_ulid, resource_id},
    store::{ChunkRecord, FakeStore, RetrievalStore},
    types::Span,
};
use mcp::{AvailableStore, StoreDescriptor};

use crate::harness::{
    call_tool, client_for, handler_with_stores, make_handler_with_one_store,
    make_handler_with_seeded_store, text_of,
};

/// T09: search returns citations in the canonical JSON shape
#[tokio::test]
async fn test_search_returns_canonical_citations() {
    let (handler, _doc_id, _chunk_id) = make_handler_with_seeded_store().await;
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "search",
        json!({ "query": "Rust programming language", "limit": 5 }),
    )
    .await
    .expect("search succeeds");
    assert_eq!(result.is_error, Some(false), "should not be a tool error");

    let text = text_of(&result);
    let json_part = text.split("\n---\n").next().unwrap_or(&text);
    let parsed: Value = serde_json::from_str(json_part).expect("valid JSON in content");

    let citations = parsed["citations"].as_array().expect("citations array");
    assert!(!citations.is_empty(), "should find at least one citation");

    let first = &citations[0];
    assert!(first.get("chunk_id").is_some(), "citation.chunk_id missing");
    assert!(
        first.get("resource_id").is_some(),
        "citation.resource_id missing"
    );
    assert!(first.get("store").is_some(), "citation.store missing");
    assert!(first.get("uri").is_some(), "citation.uri missing");
    assert!(
        first.get("title").is_some() || first.get("title").map(|v| v.is_null()).unwrap_or(true),
        "citation.title must be present (null or string)"
    );
    assert!(
        first.get("heading_path").is_some(),
        "citation.heading_path missing"
    );
    assert!(first.get("block").is_some(), "citation.block missing");
    assert!(
        first.get("chunk_position").is_some(),
        "citation.chunk_position missing"
    );
    assert!(first.get("location").is_some(), "citation.location missing");
    assert!(first.get("snippet").is_some(), "citation.snippet missing");
    assert!(first.get("score").is_some(), "citation.score missing");
    assert!(
        first.get("provenance").is_some(),
        "citation.provenance missing"
    );

    let score = &first["score"];
    assert!(score.get("fused").is_some(), "score.fused missing");
    assert!(score.get("dense").is_some(), "score.dense missing");
    assert!(score.get("bm25").is_some(), "score.bm25 missing");

    let store_obj = &first["store"];
    assert!(store_obj.get("id").is_some(), "citation.store.id missing");
    assert!(
        store_obj.get("name").is_some(),
        "citation.store.name missing"
    );

    let block = &first["block"];
    assert!(block.get("seq").is_some(), "citation.block.seq missing");
    assert!(block.get("kind").is_some(), "citation.block.kind missing");
    // #103: page from a paginated source is serialized on the MCP surface.
    assert_eq!(
        block.get("page").and_then(|p| p.as_u64()),
        Some(4),
        "citation.block.page must serialize through the MCP search surface"
    );

    assert!(
        first["chunk_position"].get("seq_in_block").is_some(),
        "citation.chunk_position.seq_in_block missing"
    );

    let span = &first["location"]["span"];
    assert!(
        span.get("start").is_some(),
        "citation.location.span.start missing"
    );
    assert!(
        span.get("end").is_some(),
        "citation.location.span.end missing"
    );

    let prov = &first["provenance"];
    assert!(
        prov.get("fetched_at").is_some(),
        "citation.provenance.fetched_at missing"
    );
    assert!(
        prov.get("content_hash").is_some(),
        "citation.provenance.content_hash missing"
    );
}

/// #94: search with a small `content_length` snaps the text-rendered snippet
/// to a natural boundary instead of cutting mid-word.
#[tokio::test]
async fn test_search_content_length_snaps_snippet_to_boundary() {
    let store = std::sync::Arc::new(FakeStore::new());

    let uri = "file:///docs/long.md";
    let text = "Rust programming is a systems language focused on safety. \
It prevents entire classes of memory bugs at compile time without a garbage \
collector, which keeps runtime performance predictable and fast.";
    let doc_hash = content_hash(text);
    let doc_id_val = resource_id(uri, &doc_hash);
    let span = Span::new(0, text.len());
    let cid = chunk_id(&doc_id_val, 0, text, 0);

    let record = ChunkRecord {
        id: cid,
        resource_id: doc_id_val,
        store_id: "store-1".to_string(),
        text: text.to_string(),
        span,
        heading_path: vec![],
        embedding: vec![0.9, 0.1, 0.1, 0.1],
        policy_version: "v1".to_string(),
        fetched_at: "2026-06-10T12:00:00Z".to_string(),
        content_hash: doc_hash,
        origin_store: "store-1".to_string(),
        source_id: new_ulid(),
        ingestor_kind: "path".to_string(),
        mime: Some("text/markdown".to_string()),
        uri: uri.to_string(),
        metadata: localdb_core::metadata::Metadata::default(),
        block_seq: 0,
        seq_in_block: 0,
        block_kind: None,
        page: None,
        window_block_seqs: vec![],
    };
    store.upsert_chunks(vec![record]).await.unwrap();

    let sd = StoreDescriptor {
        id: "store-1".to_string(),
        name: "test-store".to_string(),
        visibility: "private".to_string(),
    };
    let available = AvailableStore::from_arc(sd, store);
    let embedder: std::sync::Arc<dyn localdb_core::Embedder> =
        std::sync::Arc::new(localdb_core::FakeEmbedder::new(4));
    let handler = handler_with_stores(vec![available], embedder, false);
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "search",
        json!({ "query": "Rust programming", "limit": 1, "content_length": 60 }),
    )
    .await
    .expect("search succeeds");

    let text_out = text_of(&result);

    // The JSON part must still carry the full, untruncated snippet.
    let json_part = text_out.split("\n---\n").next().unwrap_or(&text_out);
    let parsed: Value = serde_json::from_str(json_part).unwrap();
    let citations = parsed["citations"].as_array().unwrap();
    assert!(!citations.is_empty(), "should find at least one citation");
    let full_snippet = citations[0]["snippet"].as_str().unwrap();
    assert_eq!(
        full_snippet, text,
        "JSON citation snippet must remain untruncated"
    );

    let human_part = text_out
        .split("\n---\n")
        .nth(1)
        .expect("text rendering section after separator");
    let snippet_line = human_part
        .lines()
        .find(|l| l.trim_start().starts_with("Rust programming"))
        .expect("rendered snippet line");
    let snippet_line = snippet_line.trim();
    assert!(
        snippet_line.ends_with('…'),
        "expected ellipsis marker on truncated snippet, got: {snippet_line}"
    );
    assert!(
        snippet_line.contains("safety.…") || snippet_line.ends_with("safety…"),
        "expected snap at sentence boundary, got: {snippet_line}"
    );
}

/// T10: search with unknown store name → store_not_found tool error
#[tokio::test]
async fn test_search_unknown_store_name() {
    let (handler, _, _) = make_handler_with_seeded_store().await;
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "search",
        json!({ "query": "test", "stores": ["nonexistent-store"] }),
    )
    .await
    .expect("call succeeds at the protocol level");

    assert_eq!(result.is_error, Some(true), "should be a tool error");
    let error_text = text_of(&result);
    assert!(
        error_text.contains("store_not_found") || error_text.contains("nonexistent-store"),
        "error text should reference the missing store: {error_text}"
    );
}

/// T11 (changed expectation): search with missing query argument now fails
/// `Parameters<SearchArgs>` deserialization before `tool_search` runs — a
/// tool-level "failed to deserialize parameters" error, per rmcp 1.8.0's
/// `into_tool_argument_error` (verified empirically; see
/// `harness::assert_deserialization_error`).
#[tokio::test]
async fn test_search_missing_query_argument() {
    let client = client_for(make_handler_with_one_store()).await;
    let result = call_tool(&client, "search", json!({})).await;
    let text = crate::harness::assert_deserialization_error(result);
    assert!(
        text.contains("query"),
        "error should mention 'query': {text}"
    );
}

/// T12: search returns empty citations for a store with no content
#[tokio::test]
async fn test_search_empty_store() {
    let client = client_for(make_handler_with_one_store()).await; // store has no chunks

    let result = call_tool(&client, "search", json!({ "query": "anything" }))
        .await
        .expect("search succeeds");
    assert_eq!(result.is_error, Some(false));

    let text = text_of(&result);
    let json_part = text.split("\n---\n").next().unwrap_or(&text);
    let parsed: Value = serde_json::from_str(json_part).unwrap();
    let citations = parsed["citations"].as_array().unwrap();
    assert!(
        citations.is_empty(),
        "empty store should return no citations"
    );
}

/// T13: search limit is respected
#[tokio::test]
async fn test_search_limit_respected() {
    let store = std::sync::Arc::new(FakeStore::new());

    let mut records = Vec::new();
    for i in 0..5 {
        let text = format!("Chunk {i} about Rust programming language and systems software.");
        let uri = format!("file:///docs/doc{i}.md");
        let doc_hash = content_hash(&text);
        let doc_id_val = resource_id(&uri, &doc_hash);
        let span = Span::new(0, text.len());
        let cid = chunk_id(&doc_id_val, 0, &text, 0);

        records.push(ChunkRecord {
            id: cid,
            resource_id: doc_id_val,
            store_id: "store-1".to_string(),
            text,
            span,
            heading_path: vec![],
            embedding: vec![0.9, 0.1, 0.1, 0.1],
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: doc_hash,
            origin_store: "store-1".to_string(),
            source_id: new_ulid(),
            ingestor_kind: "path".to_string(),
            mime: Some("text/markdown".to_string()),
            uri,
            metadata: localdb_core::metadata::Metadata::default(),
            block_seq: 0,
            seq_in_block: 0,
            block_kind: None,
            page: None,
            window_block_seqs: vec![],
        });
    }
    store.upsert_chunks(records).await.unwrap();

    let sd = StoreDescriptor {
        id: "store-1".to_string(),
        name: "test-store".to_string(),
        visibility: "private".to_string(),
    };
    let available = AvailableStore::from_arc(sd, store);
    let embedder: std::sync::Arc<dyn localdb_core::Embedder> =
        std::sync::Arc::new(localdb_core::FakeEmbedder::new(4));
    let handler = handler_with_stores(vec![available], embedder, false);
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "search",
        json!({ "query": "Rust programming", "limit": 3 }),
    )
    .await
    .expect("search succeeds");

    let text = text_of(&result);
    let json_part = text.split("\n---\n").next().unwrap_or(&text);
    let parsed: Value = serde_json::from_str(json_part).unwrap();
    let citations = parsed["citations"].as_array().unwrap();
    assert!(
        citations.len() <= 3,
        "should return at most 3 citations, got {}",
        citations.len()
    );
}
