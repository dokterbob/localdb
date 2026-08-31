//! `run_job` over a real feed source: the `SourceRow` gains
//! `feed_etag`/`feed_last_modified` after a 200, and is left untouched after
//! a bare 304 — the persistence hop mirroring the `policy_version`
//! self-heal in `policy_version_persistence.rs`, but for
//! `sources.feed_etag`/`feed_last_modified`.
//!
//! Exercised against a real local HTTP origin (`wiremock`) rather than a
//! scripted fetcher: `JobExecDeps::fetchers` is a concrete `HttpUrlFetcher`
//! pair, not a trait object, so there is no seam to inject a fake through at
//! this layer.

use localdb_core::ingestion::DeletionPolicy;
use localdb_core::IndexJobScope;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::common::{fake_yaml, test_state};
use crate::job_exec::{run_job, JobExecDeps};

/// Single-doc mode (`fetch_full_content: false`): the feed document is the
/// only fetch this source ever makes, keeping the mock surface to one route.
fn minimal_rss() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel><title>Feed</title><link>https://feed.example.com/</link><description>d</description></channel></rss>"#.to_string()
}

#[tokio::test]
async fn run_job_updates_source_row_after_200_and_leaves_it_untouched_after_bare_304() {
    let server = MockServer::start().await;
    let feed_url = format!("{}/feed.xml", server.uri());

    let (dir, state) = test_state().await;
    state.add_store("notes", "private").await.unwrap();
    let source = state
        .add_source(
            "notes",
            "feed",
            serde_json::json!({
                "url": feed_url,
                "fetch_full_content": false,
            }),
            "prose",
            None,
        )
        .await
        .unwrap();

    let store = state
        .backend()
        .get_store_by_name("notes")
        .await
        .unwrap()
        .unwrap();
    let yaml = fake_yaml();

    // --- Run 1: a fresh 200 carrying an ETag. ---
    Mock::given(method("GET"))
        .and(path("/feed.xml"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"v1\"")
                .set_body_string(minimal_rss()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let (stats, _embedder) = run_job(
        &store,
        IndexJobScope::Store,
        DeletionPolicy::Retain,
        JobExecDeps {
            backend: state.backend(),
            yaml: &yaml,
            models_dir: state.models_dir(),
            embedder: None,
            fetchers: None,
            progress: None,
            on_source_error: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(stats.error_count, 0, "run 1 must ingest cleanly");

    let row_after_200 = state
        .backend()
        .get_source(&source.id)
        .await
        .unwrap()
        .expect("source must still exist");
    assert_eq!(
        row_after_200.feed_etag.as_deref(),
        Some("\"v1\""),
        "a 200 must persist the response's ETag onto the SourceRow"
    );

    // --- Run 2: a bare 304, replaying the stored ETag. ---
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/feed.xml"))
        .and(header("if-none-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(&server)
        .await;

    let (stats, _embedder) = run_job(
        &store,
        IndexJobScope::Store,
        DeletionPolicy::Retain,
        JobExecDeps {
            backend: state.backend(),
            yaml: &yaml,
            models_dir: state.models_dir(),
            embedder: None,
            fetchers: None,
            progress: None,
            on_source_error: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(stats.error_count, 0, "run 2 must complete cleanly");

    let row_after_304 = state
        .backend()
        .get_source(&source.id)
        .await
        .unwrap()
        .expect("source must still exist");
    assert_eq!(
        row_after_304.feed_etag.as_deref(),
        Some("\"v1\""),
        "a bare 304 must leave the stored ETag exactly as it was"
    );
    assert_eq!(row_after_304.feed_last_modified, None);

    drop(dir);
}
