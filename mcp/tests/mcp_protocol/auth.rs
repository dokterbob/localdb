use super::harness::{call_tool, client_for, text_of};
use localdb_core::{
    ids::{chunk_id, content_hash, new_ulid, resource_id},
    store::{ChunkRecord, FakeStore, RetrievalStore},
    types::Span,
    FakeEmbedder,
};
use mcp::{handler::McpHandler, AvailableStore, StaticStoreProvider, StoreDescriptor};
use serde_json::{json, Value};

#[tokio::test]
async fn test_member_principal_sees_only_granted_store_via_list_stores_and_search() {
    let granted_store = std::sync::Arc::new(FakeStore::new());
    let ungranted_store = std::sync::Arc::new(FakeStore::new());
    let granted = AvailableStore::from_arc(
        StoreDescriptor {
            id: "store-granted".to_string(),
            name: "store-granted".to_string(),
            visibility: "shared".to_string(),
        },
        granted_store,
    );
    let ungranted = AvailableStore::from_arc(
        StoreDescriptor {
            id: "store-ungranted".to_string(),
            name: "store-ungranted".to_string(),
            visibility: "shared".to_string(),
        },
        ungranted_store,
    );

    let member = localdb_core::auth::Principal {
        user_id: "u-member".to_string(),
        name: "member-with-grant".to_string(),
        role: localdb_core::auth::Role::Member,
        access: localdb_core::auth::StoreAccess::Granted(
            ["store-granted".to_string()].into_iter().collect(),
        ),
    };

    let embedder: std::sync::Arc<dyn localdb_core::Embedder> =
        std::sync::Arc::new(FakeEmbedder::new(4));
    let stores = vec![granted, ungranted];
    let backend = std::sync::Arc::new(mcp::tools::StoresBackend::new(&stores));
    let handler = McpHandler::new(
        std::sync::Arc::new(StaticStoreProvider::new(stores)),
        backend,
        embedder,
        false,
        Some(member),
    );
    let client = client_for(handler).await;

    // list_stores: the ungranted shared store must be invisible.
    let result = call_tool(&client, "list_stores", json!({}))
        .await
        .expect("list_stores succeeds at the protocol level");
    assert_ne!(result.is_error, Some(true), "should not be a tool error");
    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).expect("valid JSON in content");
    let names: Vec<&str> = parsed["stores"]
        .as_array()
        .expect("stores array")
        .iter()
        .map(|s| s["name"].as_str().expect("name is a string"))
        .collect();
    assert_eq!(
        names,
        vec!["store-granted"],
        "member without a grant on store-ungranted must not see it via list_stores"
    );

    // search naming the ungranted store explicitly: forbidden, not
    // store_not_found — the store exists, the grant is just missing.
    let result = call_tool(
        &client,
        "search",
        json!({ "query": "anything", "stores": ["store-ungranted"] }),
    )
    .await
    .expect("search call completes at the protocol level");
    assert_eq!(
        result.is_error,
        Some(true),
        "search naming an ungranted store must be a tool error"
    );
    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).expect("valid JSON in content");
    assert_eq!(
        parsed["error"]["code"], "forbidden",
        "expected D7 'forbidden', not e.g. store_not_found: {text}"
    );
}

/// D7 regression test for `get_document`/`get_chunks` (mcp/src/handler.rs):
/// a scoped `Role::Member` principal holding a grant on only one of two
/// `shared` stores must not be able to read a document/chunk that lives in
/// the *other* (ungranted) store. Unlike `search`'s `stores` filter, neither
/// `get_document` nor `get_chunks` takes an explicit store name, so per
/// `McpHandler::authorize_and_resolve`'s doc comment there is nothing named
/// to be "forbidden" from: the ungranted store is dropped from the store
/// list *before* either tool runs, so an ungranted-store id looks identical
/// to an unknown one and must surface as `resource_not_found` — this
/// complements
/// `test_member_principal_sees_only_granted_store_via_list_stores_and_search`
/// (which covers `list_stores`/`search`) for the two remaining read tools.
#[tokio::test]
async fn test_member_principal_cannot_get_document_or_chunks_from_ungranted_store() {
    let granted_store = std::sync::Arc::new(FakeStore::new());
    let ungranted_store = std::sync::Arc::new(FakeStore::new());

    // Seed a retrievable document/chunk into the *ungranted* store only.
    let uri = "file:///docs/secret.md";
    let doc_hash = content_hash("confidential content the member must never see");
    let doc_id = resource_id(uri, &doc_hash);
    let snippet = "This paragraph must stay invisible to an ungranted member.";
    let span = Span::new(0, snippet.len());
    let cid = chunk_id(&doc_id, 0, snippet, 0);

    let record = ChunkRecord {
        id: cid.clone(),
        resource_id: doc_id.clone(),
        store_id: "store-ungranted".to_string(),
        text: snippet.to_string(),
        span,
        heading_path: vec!["Secret".to_string()],
        embedding: vec![0.1, 0.2, 0.3, 0.4],
        policy_version: "v1".to_string(),
        fetched_at: "2026-06-10T12:00:00Z".to_string(),
        content_hash: doc_hash.clone(),
        origin_store: "store-ungranted".to_string(),
        source_id: new_ulid(),
        ingestor_kind: "path".to_string(),
        mime: Some("text/markdown".to_string()),
        uri: uri.to_string(),
        metadata: localdb_core::metadata::Metadata::default(),
        block_seq: 0,
        seq_in_block: 0,
        block_kind: None,
        window_block_seqs: vec![],
        modified_at: None,
        page: None,
        date_original: None,
        date_parsed: None,
        external_id: None,
        external_etag: None,
    };
    ungranted_store
        .upsert_chunks(vec![record])
        .await
        .expect("seed chunk into ungranted store");

    let granted = AvailableStore::from_arc(
        StoreDescriptor {
            id: "store-granted".to_string(),
            name: "store-granted".to_string(),
            visibility: "shared".to_string(),
        },
        granted_store,
    );
    let ungranted = AvailableStore::from_arc(
        StoreDescriptor {
            id: "store-ungranted".to_string(),
            name: "store-ungranted".to_string(),
            visibility: "shared".to_string(),
        },
        ungranted_store,
    );

    let member = localdb_core::auth::Principal {
        user_id: "u-member".to_string(),
        name: "member-with-grant".to_string(),
        role: localdb_core::auth::Role::Member,
        access: localdb_core::auth::StoreAccess::Granted(
            ["store-granted".to_string()].into_iter().collect(),
        ),
    };

    let embedder: std::sync::Arc<dyn localdb_core::Embedder> =
        std::sync::Arc::new(FakeEmbedder::new(4));
    let stores = vec![granted, ungranted];
    let backend = std::sync::Arc::new(mcp::tools::StoresBackend::new(&stores));
    let handler = McpHandler::new(
        std::sync::Arc::new(StaticStoreProvider::new(stores)),
        backend,
        embedder,
        false,
        Some(member),
    );
    let client = client_for(handler).await;

    // get_document: the ungranted store's document must not be returned to
    // the member, and its content must not leak into the error either.
    let result = call_tool(&client, "get_document", json!({ "id": doc_id }))
        .await
        .expect("get_document completes at the protocol level");
    assert_eq!(
        result.is_error,
        Some(true),
        "member without a grant on store-ungranted must not be able to read its document"
    );
    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).expect("valid JSON in content");
    assert_eq!(
        parsed["error"]["code"], "resource_not_found",
        "an ungranted-store document has no store name to be 'forbidden' from, so it looks \
         identical to an unknown id per handler.rs's authorize_and_resolve doc comment: {text}"
    );
    assert!(
        !text.contains(snippet),
        "the ungranted store's document text must never leak into the response: {text}"
    );

    // get_chunks: same expectation, on the same ungranted-store resource.
    let result = call_tool(&client, "get_chunks", json!({ "resource_id": doc_id }))
        .await
        .expect("get_chunks completes at the protocol level");
    assert_eq!(
        result.is_error,
        Some(true),
        "member without a grant on store-ungranted must not be able to read its chunks"
    );
    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).expect("valid JSON in content");
    assert_eq!(
        parsed["error"]["code"], "resource_not_found",
        "an ungranted-store resource_id has no store name to be 'forbidden' from: {text}"
    );
    assert!(
        !text.contains(snippet),
        "the ungranted store's chunk text must never leak into the response: {text}"
    );
}
