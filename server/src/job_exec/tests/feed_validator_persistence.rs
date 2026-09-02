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

use localdb_core::config::schema::{
    DefaultsConfig, EmbeddingPolicy, IndexingPolicyConfig, RawConfig,
};
use localdb_core::ingestion::DeletionPolicy;
use localdb_core::IndexJobScope;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::common::{fake_yaml, test_state};
use crate::job_exec::{run_job, JobExecDeps};

/// `fake_yaml()` with one chunking knob moved, which is all it takes to
/// change `policy_version` — the change an origin has no way to hear about.
fn fake_yaml_with_other_policy() -> RawConfig {
    let mut chunking = localdb_core::config::schema::ChunkingPolicy::default();
    chunking
        .preset_overrides
        .insert("prose".to_string(), "code".to_string());
    RawConfig {
        defaults: DefaultsConfig {
            indexing: IndexingPolicyConfig {
                chunking,
                embedding: EmbeddingPolicy {
                    provider: "fake".to_string(),
                    model: "default".to_string(),
                },
                ..Default::default()
            },
        },
        ..Default::default()
    }
}

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

/// The regression the `sources.feed_inputs_digest` gate exists for, at the
/// job surface where it actually bites.
///
/// A conditional GET rests on the origin rotating its validator when *its*
/// representation changes. Our indexing policy is not part of that
/// representation and the origin cannot see it, so before the gate a policy
/// change against an unchanged feed produced a 304, the entry loop never
/// ran, and nothing was ever reprocessed under the new policy — for as long
/// as the feed XML held still, which for a dormant feed is forever.
///
/// Three runs: capture, prove the replay works under unchanged inputs, then
/// prove a policy change suppresses it. The middle run matters as much as
/// the last — without it a gate that simply never replayed anything would
/// pass this test.
#[tokio::test]
async fn a_policy_change_forces_an_unconditional_feed_fetch() {
    let server = MockServer::start().await;
    let feed_url = format!("{}/feed.xml", server.uri());

    let (dir, state) = test_state().await;
    state.add_store("notes", "private").await.unwrap();
    let source = state
        .add_source(
            "notes",
            "feed",
            serde_json::json!({ "url": feed_url, "fetch_full_content": false }),
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

    // Answers 304 to any conditional request and 200 otherwise, so which
    // branch a run took is readable off the recorded request headers rather
    // than inferred from a count.
    async fn mount_feed(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/feed.xml"))
            .and(header("if-none-match", "\"v1\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/feed.xml"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"v1\"")
                    .set_body_string(minimal_rss()),
            )
            .mount(server)
            .await;
    }
    mount_feed(&server).await;

    async fn index(
        state: &crate::state::AppState,
        store: &localdb_core::StoreRow,
        yaml: &RawConfig,
    ) {
        run_job(
            store,
            IndexJobScope::Store,
            DeletionPolicy::Retain,
            JobExecDeps {
                backend: state.backend(),
                yaml,
                models_dir: state.models_dir(),
                embedder: None,
                fetchers: None,
                progress: None,
                on_source_error: None,
            },
        )
        .await
        .unwrap();
    }

    /// Whether the most recent `/feed.xml` request carried `If-None-Match`.
    async fn last_request_was_conditional(server: &MockServer) -> bool {
        let reqs = server.received_requests().await.expect("recording is on");
        reqs.last()
            .expect("at least one request")
            .headers
            .contains_key("if-none-match")
    }

    // --- Run 1: nothing stored, so nothing to replay. ---
    let yaml = fake_yaml();
    index(&state, &store, &yaml).await;
    assert!(!last_request_was_conditional(&server).await);
    let row = state
        .backend()
        .get_source(&source.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.feed_etag.as_deref(), Some("\"v1\""));
    let captured_digest = row
        .feed_inputs_digest
        .clone()
        .expect("the inputs behind those validators must be recorded with them");

    // --- Run 2: same policy, so the cache is trusted and replayed. ---
    index(&state, &store, &yaml).await;
    assert!(
        last_request_was_conditional(&server).await,
        "unchanged inputs must still replay the stored validator — otherwise \
         the gate is just disabling conditional GET"
    );

    // --- Run 3: policy moved; the origin cannot know, so we must not ask. ---
    let other_yaml = fake_yaml_with_other_policy();
    index(&state, &store, &other_yaml).await;
    assert!(
        !last_request_was_conditional(&server).await,
        "a policy change must force an unconditional fetch: a 304 here would \
         skip the entry loop and strand every entry on the old policy"
    );

    let row = state
        .backend()
        .get_source(&source.id)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        row.feed_inputs_digest.as_deref(),
        Some(captured_digest.as_str()),
        "the run must record the digest of the inputs it actually used, or the \
         mismatch would repeat forever and the cache would never come back"
    );

    drop(dir);
}

/// A run that could not index an entry must not store the feed document's
/// validators.
///
/// The feed XML that lists the failed entry is unchanged, so storing its
/// validators would let the next run answer 304, skip the entry loop
/// entirely, and never retry — the entry stranded until the feed document
/// itself changes, which for an aging entry can be indefinitely. Withholding
/// them costs a full feed-document refetch each run while the entry is
/// broken; the entries' own resource-level validators still spare the
/// expensive half of the work.
///
/// The failure used here is the one this layer can actually produce: the
/// entry fetcher is `new_public_only`, so a link pointing at the loopback
/// mock is refused by the destination guard, and an entry with no summary,
/// content or title has nothing to fall back to. That is a genuine partial
/// pass — the run read the feed fine and still left an entry unindexed —
/// which is exactly the condition the gate keys on.
///
/// Three runs' worth of assertions, because the middle one is what makes the
/// last meaningful: fail and withhold, prove the *next* run really is
/// unconditional, then let it succeed and prove the validators land after all.
#[tokio::test]
async fn a_run_that_could_not_index_an_entry_withholds_the_feed_validators() {
    let server = MockServer::start().await;
    let feed_url = format!("{}/feed.xml", server.uri());
    let entry_url = format!("{}/entry-1", server.uri());
    // No <title>, <description> or <content>: nothing to fall back to once
    // the link is refused.
    let rss_without_content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel><title>Feed</title><link>{server_uri}/</link><description>d</description><item><link>{entry_url}</link><guid>{entry_url}</guid></item></channel></rss>"#,
        server_uri = server.uri(),
    );
    // Same feed, same entry, now carrying a summary the entry can be indexed
    // from.
    let rss_with_summary = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel><title>Feed</title><link>{server_uri}/</link><description>d</description><item><title>E1</title><link>{entry_url}</link><guid>{entry_url}</guid><description>A summary long enough to chunk into something.</description></item></channel></rss>"#,
        server_uri = server.uri(),
    );

    let (dir, state) = test_state().await;
    state.add_store("notes", "private").await.unwrap();
    let source = state
        .add_source(
            "notes",
            "feed",
            serde_json::json!({ "url": feed_url, "fetch_full_content": true }),
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

    macro_rules! run {
        () => {
            run_job(
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
            .unwrap()
        };
    }

    // --- Run 1: the feed reads fine, its one entry cannot be indexed. ---
    Mock::given(method("GET"))
        .and(path("/feed.xml"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"feed-v1\"")
                .set_body_string(rss_without_content),
        )
        .mount(&server)
        .await;

    let (stats, _embedder) = run!();
    assert_eq!(
        stats.error_count, 1,
        "the unindexable entry must count as an error"
    );
    assert_eq!(stats.docs_indexed, 0);

    let row = state
        .backend()
        .get_source(&source.id)
        .await
        .unwrap()
        .expect("source must still exist");
    assert_eq!(
        row.feed_etag, None,
        "a run that left an entry unindexed must not store the feed document's ETag"
    );
    assert_eq!(
        row.feed_inputs_digest, None,
        "the digest is written with the validators or not at all"
    );

    // --- Run 2: therefore unconditional. The `expect(0)` mock fails the
    // test if anything goes out carrying `If-None-Match`. ---
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/feed.xml"))
        .and(wiremock::matchers::header_exists("if-none-match"))
        .respond_with(ResponseTemplate::new(304))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/feed.xml"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"feed-v1\"")
                .set_body_string(rss_with_summary),
        )
        .mount(&server)
        .await;

    let (stats, _embedder) = run!();
    assert_eq!(stats.error_count, 0, "run 2 must ingest cleanly");
    assert_eq!(
        stats.docs_indexed, 1,
        "the entry finally indexes, from its summary"
    );

    // --- And now, the run having succeeded, the validators land. ---
    let row = state
        .backend()
        .get_source(&source.id)
        .await
        .unwrap()
        .expect("source must still exist");
    assert_eq!(
        row.feed_etag.as_deref(),
        Some("\"feed-v1\""),
        "a clean run must persist the feed document's ETag"
    );
    assert!(
        row.feed_inputs_digest.is_some(),
        "the digest rides along with the validators"
    );

    drop(dir);
}
