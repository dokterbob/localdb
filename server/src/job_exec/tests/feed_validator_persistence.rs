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
        false,
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
        false,
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
            false,
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

/// T329: `--refetch` (`SourceIngestionDeps::refetch`) suppresses the feed
/// document's own stored conditional-GET validators for the run, the same
/// effect a `feed_inputs_digest` mismatch has above — `refetch: false`
/// still replays a stored validator as a conditional GET; `refetch: true`
/// fetches unconditionally instead.
#[tokio::test]
async fn refetch_true_bypasses_stored_feed_validators_refetch_false_still_replays_them() {
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
    let yaml = fake_yaml();

    async fn index(
        state: &crate::state::AppState,
        store: &localdb_core::StoreRow,
        yaml: &RawConfig,
        refetch: bool,
    ) {
        run_job(
            store,
            IndexJobScope::Store,
            DeletionPolicy::Retain,
            refetch,
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

    // Answers 304 to a conditional request carrying the stored ETag, and 200
    // otherwise — so which branch a run took is readable off the recorded
    // request headers.
    Mock::given(method("GET"))
        .and(path("/feed.xml"))
        .and(header("if-none-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/feed.xml"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"v1\"")
                .set_body_string(minimal_rss()),
        )
        .mount(&server)
        .await;

    // --- Run 1: nothing stored yet, so nothing to replay. ---
    index(&state, &store, &yaml, false).await;
    assert!(!last_request_was_conditional(&server).await);
    let row = state
        .backend()
        .get_source(&source.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.feed_etag.as_deref(), Some("\"v1\""));

    // --- Run 2: refetch: false still replays the stored validator. ---
    index(&state, &store, &yaml, false).await;
    assert!(
        last_request_was_conditional(&server).await,
        "refetch: false must still replay the stored validator as a conditional GET"
    );

    // --- Run 3: refetch: true suppresses it. ---
    index(&state, &store, &yaml, true).await;
    assert!(
        !last_request_was_conditional(&server).await,
        "refetch: true must bypass the stored validator and fetch unconditionally"
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
                false,
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

/// The withhold guard's other trigger (specs/04-search-pipeline.md §1
/// "Due-entry revisit on a feed 304"): a feed 304 can itself rotate a
/// validator (RFC 9111 permits, and requires storing, a fresh validator on a
/// 304 response), which alone makes `document_validators = Some(..)`. That
/// same run's due-entry loop — the one a 304 triggers — can independently
/// leave an entry unindexed (no embedded-content fallback exists on that
/// path, per `process_due_recheck_entry`'s doc comment in
/// `ingest::feed_ingestor`). The pre-existing `r.error_count > 0` guard above
/// is not specific to how the error arose, so it must withhold the rotated
/// validator here exactly as
/// `a_run_that_could_not_index_an_entry_withholds_the_feed_validators` proves
/// it does for an ordinary discovery-loop failure — this is the scenario
/// Codex's F-304-STARVATION review finding named specifically (a partial run
/// must not let the next run 304 its way past a still-broken entry).
#[tokio::test]
async fn a_due_entry_loop_error_on_a_feed_304_withholds_the_rotated_validator() {
    let server = MockServer::start().await;
    let feed_url = format!("{}/feed.xml", server.uri());
    let entry_url = format!("{}/entry-1", server.uri());
    // A `<description>` so run 1 indexes the entry via its embedded-content
    // fallback: the link is a loopback address, refused by the entry
    // fetcher's public-destination-only guard exactly as
    // `feed_liveness_sweep.rs`'s tests rely on for the same reason.
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
                false,
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

    // --- Run 1: the feed reads fine and its one entry indexes cleanly via
    // its embedded-content fallback. Under T329's fallback-stamp fix
    // (`on_resource_fallback` never calls `touch_resource_checked`), the
    // entry's `last_checked_at` stays unset afterward — so it is already a
    // due-entry candidate for run 2's revisit loop, with no explicit
    // backdating needed (mirroring `feed_liveness_sweep.rs`'s identical run-1
    // shape and its own comment on this point).
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
    assert_eq!(stats.error_count, 0, "run 1 must ingest cleanly");
    assert_eq!(
        stats.docs_indexed, 1,
        "the entry must index via its embedded-content fallback"
    );

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

    // --- Run 2: the feed answers 304 but rotates its ETag on the 304 itself
    // (`FeedIngestor`'s `FetchResult::NotModified` handling folds a rotated
    // validator into `document_validators` even though the body is
    // unchanged). That alone would normally persist the new ETag — except
    // the due-entry loop this 304 triggers revisits the entry backdated by
    // nothing at all (its `last_checked_at` was never stamped in run 1), and
    // its link is refused by the same destination guard — this time with no
    // embedded fallback available, since the feed body was never re-fetched
    // this run — so it counts as an error.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/feed.xml"))
        .and(header("if-none-match", "\"feed-v1\""))
        .respond_with(ResponseTemplate::new(304).insert_header("etag", "\"feed-v2\""))
        .mount(&server)
        .await;

    let (stats, _embedder) = run!();
    assert_eq!(
        stats.error_count, 1,
        "the due-entry loop's blocked outcome (no embedded fallback on this path) must count \
         as an error"
    );
    assert_eq!(
        stats.docs_indexed, 0,
        "run 2's feed 304 means the ordinary discovery loop never runs at all"
    );

    let row = state
        .backend()
        .get_source(&source.id)
        .await
        .unwrap()
        .expect("source must still exist");
    assert_eq!(
        row.feed_etag.as_deref(),
        Some("\"feed-v1\""),
        "the rotated ETag from run 2's 304 must be withheld, not persisted, because the \
         due-entry loop it triggered left an entry unindexed"
    );

    drop(dir);
}
