use super::citation_headline;
use localdb_core::citation::{
    ChunkPosition, Citation, CitationBlock, CitationLocation, CitationProvenance, CitationStore,
    Score,
};
use localdb_core::types::Span;

fn citation_with(page: Option<u32>, heading: Vec<String>) -> Citation {
    Citation {
        duplicates: vec![],
        chunk_id: "chunk".to_string(),
        resource_id: "res".to_string(),
        store: CitationStore {
            id: "01HN1Y28MYWN6X5DSKZMNE1T5W".to_string(),
            name: "s".to_string(),
        },
        uri: "file:///docs/paper.pdf".to_string(),
        title: None,
        heading_path: heading,
        block: CitationBlock {
            seq: 0,
            kind: Some("text".to_string()),
            page,
        },
        chunk_position: ChunkPosition { seq_in_block: 0 },
        location: CitationLocation {
            span: Span::new(0, 4),
            window_block_seqs: vec![],
        },
        snippet: "text".to_string(),
        score: Score {
            fused: 1.0,
            dense: None,
            bm25: None,
        },
        provenance: CitationProvenance {
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: "abc".to_string(),
        },
        metadata: Default::default(),
    }
}

#[test]
fn headline_appends_page_when_present() {
    let line = citation_headline(&citation_with(Some(12), vec![]));
    assert_eq!(line, "file:///docs/paper.pdf (p.12)");
}

#[test]
fn headline_omits_page_when_absent() {
    let line = citation_headline(&citation_with(None, vec![]));
    assert_eq!(line, "file:///docs/paper.pdf");
}

#[test]
fn headline_combines_heading_path_and_page() {
    let line = citation_headline(&citation_with(
        Some(3),
        vec!["Intro".to_string(), "Setup".to_string()],
    ));
    assert_eq!(line, "file:///docs/paper.pdf > Intro > Setup (p.3)");
}

#[tokio::test]
async fn capabilities_use_one_probe_and_off_bypasses_only_dedup() {
    use localdb_core::SearchDedup;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    let app = axum::Router::new().route(
        "/v1/status",
        axum::routing::get(move || {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                axum::Json(serde_json::json!({"features":["search_filters", "search_dedup"]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let ctx = crate::tests::context();
    super::require_daemon_search_support(&ctx, &url, false, SearchDedup::Off)
        .await
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 0);
    super::require_daemon_search_support(&ctx, &url, true, SearchDedup::TextAndVector)
        .await
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    super::require_daemon_search_support(&ctx, &url, true, SearchDedup::Off)
        .await
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn old_daemon_rejects_default_grouping_but_accepts_off() {
    let app = axum::Router::new().route(
        "/v1/status",
        axum::routing::get(|| async { axum::Json(serde_json::json!({})) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let ctx = crate::tests::context();
    for mode in [
        localdb_core::SearchDedup::Text,
        localdb_core::SearchDedup::TextAndVector,
    ] {
        let error = super::require_daemon_search_support(&ctx, &url, false, mode)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            localdb_core::Error::DaemonCapabilityUnavailable { .. }
        ));
        assert!(error.to_string().contains("--dedup off"));
    }
    super::require_daemon_search_support(&ctx, &url, false, localdb_core::SearchDedup::Off)
        .await
        .unwrap();
    assert!(
        super::require_daemon_search_support(&ctx, &url, true, localdb_core::SearchDedup::Off)
            .await
            .is_err()
    );
    server.abort();
}
