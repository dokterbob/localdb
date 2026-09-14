//! `run_job` over a real feed source, driving the liveness sweep
//! (specs/04-search-pipeline.md §1 "Aged-out feed entries: the liveness
//! sweep") through to a real local HTTP origin — the security-relevant
//! check for this stage.
//!
//! `wiremock` binds to loopback, which is exactly the kind of destination
//! `fetch::HttpUrlFetcher::new_public_only`'s guard exists to refuse (it
//! accepts only globally-routable destinations) — see `fetch/src/lib.rs`'s
//! own "wiremock's loopback binding is the asset" test for the same
//! property in isolation. That makes loopback a reliable oracle here: if
//! the liveness sweep's probe of an aged-out entry comes back `Blocked`
//! rather than actually reaching wiremock's canned 404, the fetcher
//! `job_exec::run_job` wired into the sweep is the restricted
//! `entry_fetcher`, not the unrestricted `url_fetcher` it also holds (the
//! SSRF regression this stage's brief calls out by name). Had `run_job`
//! mistakenly passed `url_fetcher` instead, this same test would instead
//! observe a real 404 and an actual delete — so this test fails loudly on
//! exactly that regression, rather than passing either way.
//!
//! Exercised against a real local HTTP origin (`wiremock`), like
//! `feed_validator_persistence.rs`: `JobExecDeps::fetchers` is a concrete
//! `HttpUrlFetcher` pair, not a trait object, so there is no seam to inject
//! a fake through at this layer.

use localdb_core::ingestion::DeletionPolicy;
use localdb_core::{IndexJobScope, IndexJobStats};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::common::{fake_yaml, test_state, test_state_with_backend};
use crate::job_exec::{run_job, JobExecDeps};
use crate::state::AppState;

fn rss_with_entries(entries_xml: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel><title>Feed</title><link>https://feed.example.com/</link><description>d</description>{entries_xml}</channel></rss>"#
    )
}

fn item(title: &str, link: &str, guid: &str) -> String {
    format!(
        r#"<item><title>{title}</title><link>{link}</link><guid>{guid}</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Body for {title}.</description></item>"#
    )
}

async fn run_once(
    state: &AppState,
    yaml: &localdb_core::config::schema::RawConfig,
    store: &localdb_core::StoreRow,
    deletion: DeletionPolicy,
) -> IndexJobStats {
    let (stats, _embedder) = run_job(
        store,
        IndexJobScope::Store,
        deletion,
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
    stats
}

async fn seed_two_entry_feed(state: &AppState, feed_url: &str) {
    state.add_store("notes", "private").await.unwrap();
    state
        .add_source(
            "notes",
            "feed",
            serde_json::json!({
                "url": feed_url,
                "fetch_full_content": true,
            }),
            "prose",
            None,
        )
        .await
        .unwrap();
}

/// `--delete`: the liveness sweep reaches the real `entry_fetcher` for an
/// aged-out entry, and that fetcher's destination guard — not a stub —
/// blocks the loopback probe. See this module's doc comment for why
/// `Blocked` here, not a followed 404, is the proof that the *restricted*
/// fetcher is what `run_job` wired in.
#[tokio::test]
async fn aged_out_entry_probe_goes_through_the_restricted_entry_fetcher() {
    let server = MockServer::start().await;
    let feed_url = format!("{}/feed.xml", server.uri());
    let entry_a_url = format!("{}/entry-a", server.uri());
    let entry_b_url = format!("{}/entry-b", server.uri());

    // `test_state_with_backend` (not plain `test_state`) because this test
    // must reach past `AppState::backend()`'s `&dyn StoreBackend` to
    // backdate a resource's `last_checked_at` below — see the comment at
    // that backdating call for why.
    let (dir, state, sqlite_backend) = test_state_with_backend().await;
    seed_two_entry_feed(&state, &feed_url).await;
    let store = state
        .backend()
        .get_store_by_name("notes")
        .await
        .unwrap()
        .unwrap();
    let yaml = fake_yaml();

    // Run 1: both entries are inside the feed's window. Their links are
    // never actually reachable in this test (loopback, blocked by
    // `entry_fetcher`'s guard just like the sweep's own probe below), so
    // both fall back to the feed-embedded `<description>` — the same
    // fallback `feed_ingestor::tests::feed_entry_link_blocked_falls_back_to_embedded_content`
    // pins at the unit level. `docs_indexed == 2` below only needs that
    // fallback to work, not the link to be reachable.
    Mock::given(method("GET"))
        .and(path("/feed.xml"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(rss_with_entries(&format!(
                "{}{}",
                item("Entry A", &entry_a_url, "guid-a"),
                item("Entry B", &entry_b_url, "guid-b"),
            ))),
        )
        .mount(&server)
        .await;

    let stats1 = run_once(&state, &yaml, &store, DeletionPolicy::Prune).await;
    assert_eq!(stats1.error_count, 0, "run 1 must ingest cleanly");
    assert_eq!(
        stats1.docs_indexed, 2,
        "both entries must be indexed on run 1, via the embedded-content fallback"
    );

    // Both entries were indexed via the embedded-content fallback (their
    // links are blocked, never a real origin contact), so `on_resource_fallback`
    // — not `on_resource` — is what ran for them, and it never calls
    // `touch_resource_checked` (specs/04-search-pipeline.md §1 "Recheck
    // gate": only a real 200/304 origin contact stamps `last_checked_at`).
    // Confirm that end to end through the real libsql store before relying
    // on it: `last_checked_at IS NULL` is exactly what already makes
    // `list_stale_feed_resources` treat a never-checked row as a candidate
    // (see that query's own doc comment), which is what the rest of this
    // test's backdating step below is set up to demonstrate explicitly.
    let retrieval_store = state.backend().retrieval_store(&store.id).await.unwrap();
    let indexed_after_run1 = retrieval_store.list_indexed_documents().await.unwrap();
    let entry_a_record = indexed_after_run1
        .iter()
        .find(|doc| doc.uri == entry_a_url)
        .expect("entry A must have been indexed by run 1");
    assert!(
        indexed_after_run1
            .iter()
            .all(|doc| doc.last_checked_at.is_none()),
        "embedded-content fallback must never stamp last_checked_at for either entry"
    );

    // Explicitly backdating (rather than relying on the NULL left by run 1)
    // is what makes "A scrolled off the feed window a while ago" true in
    // wall-clock terms, on top of "A is absent from this run's feed"
    // (already true via the narrowed mock below) — a fixed past timestamp
    // pins the sweep's candidate-selection behaviour on a stale
    // `last_checked_at`, not merely a `NULL` one, so this test still proves
    // something once a later fix (e.g. remembering fallback failures) stops
    // leaving it `NULL`.
    sqlite_backend
        .set_last_checked_at_for_test(
            &store.id,
            &entry_a_record.resource_id,
            Some("2000-01-01T00:00:00Z"),
        )
        .await
        .unwrap();

    // Run 2: the feed narrows to just B — A has scrolled off the window,
    // so the liveness sweep (not the ordinary discovery loop, which never
    // sees A at all this run) is what reaches its link.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/feed.xml"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(rss_with_entries(&item(
                "Entry B",
                &entry_b_url,
                "guid-b",
            ))),
        )
        .mount(&server)
        .await;
    // `/entry-a` answering 404 here is deliberately never reachable over
    // the wire: `new_public_only`'s preflight IP-literal check refuses a
    // loopback destination before any request is sent, so `expect(0)`
    // asserts the request never leaves the process — the strongest
    // available proof, in this environment, that the sweep used the
    // restricted fetcher.
    Mock::given(method("GET"))
        .and(path("/entry-a"))
        .respond_with(ResponseTemplate::new(404))
        .expect(0)
        .mount(&server)
        .await;

    let stats2 = run_once(&state, &yaml, &store, DeletionPolicy::Prune).await;
    assert_eq!(stats2.error_count, 0, "run 2 must complete cleanly");
    assert_eq!(
        stats2.feed_entries_liveness_checked, 1,
        "exactly one candidate (the aged-out entry) must be probed"
    );
    assert_eq!(
        stats2.docs_deleted, 0,
        "a Blocked destination is neither evidence of anything nor a followed 404 — \
         the resource must be left untouched, not deleted"
    );

    drop(dir);
}

/// The default path: without `--delete`, the liveness sweep must never run
/// at all — zero probes, zero deletes, and (unlike the test above) not even
/// an attempt the destination guard would have to refuse.
#[tokio::test]
async fn aged_out_entry_is_retained_and_unprobed_without_delete() {
    let server = MockServer::start().await;
    let feed_url = format!("{}/feed.xml", server.uri());
    let entry_a_url = format!("{}/entry-a", server.uri());
    let entry_b_url = format!("{}/entry-b", server.uri());

    let (dir, state) = test_state().await;
    seed_two_entry_feed(&state, &feed_url).await;
    let store = state
        .backend()
        .get_store_by_name("notes")
        .await
        .unwrap()
        .unwrap();
    let yaml = fake_yaml();

    Mock::given(method("GET"))
        .and(path("/feed.xml"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(rss_with_entries(&format!(
                "{}{}",
                item("Entry A", &entry_a_url, "guid-a"),
                item("Entry B", &entry_b_url, "guid-b"),
            ))),
        )
        .mount(&server)
        .await;

    let stats1 = run_once(&state, &yaml, &store, DeletionPolicy::Retain).await;
    assert_eq!(stats1.error_count, 0);
    assert_eq!(stats1.docs_indexed, 2);

    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/feed.xml"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(rss_with_entries(&item(
                "Entry B",
                &entry_b_url,
                "guid-b",
            ))),
        )
        .mount(&server)
        .await;
    // Registered purely so the test fails loudly (not silently) if the
    // sweep ever reaches this URL under a retaining run — `expect(0)`.
    Mock::given(method("GET"))
        .and(path("/entry-a"))
        .respond_with(ResponseTemplate::new(404))
        .expect(0)
        .mount(&server)
        .await;

    let stats2 = run_once(&state, &yaml, &store, DeletionPolicy::Retain).await;
    assert_eq!(stats2.error_count, 0);
    assert_eq!(
        stats2.docs_deleted, 0,
        "DeletionPolicy::Retain must never delete"
    );
    assert_eq!(
        stats2.feed_entries_liveness_checked, 0,
        "DeletionPolicy::Retain must perform zero liveness probes"
    );

    drop(dir);
}
