//! End-to-end tests proving `embed::DownloadProgress` actually reaches
//! `embed::create_embedder` from each of the four CLI call sites that
//! compute it via `crate::progress::download_progress_for(ctx.json)` (issue
//! #261): `cmds::init::run_init_async`, `cmds::search::SearchCmd::run_embedded`,
//! `cmds::surface::build_mcp_embedder` (the step `run_mcp_async` reaches
//! `create_embedder` through), and `job_attach::run_embedded_store_job`.
//!
//! Each test drives the real command function against a real temp
//! config/DB with `provider: fake` (offline, no model download) and asserts
//! on `embed::last_download_progress()` — a process-wide value recorded at
//! the top of `create_embedder`, before the provider match, so it fires even
//! for `provider: fake`. That is the whole point: these tests prove the
//! *value* threaded through each call site, not merely that
//! `create_embedder` compiles with five arguments or that the command
//! succeeds.
//!
//! What these tests do **not** prove: that fastembed's `indicatif` download
//! bar actually goes quiet when the `bool` fastembed derives from
//! `DownloadProgress` is `false`. That is fastembed's own contract, not
//! ours — this crate no more re-tests it than it re-tests `reqwest`'s TLS
//! handshake. A real-download test (forcing a cold model cache and
//! capturing stderr bytes) was considered and skipped: it costs ~706 MB and
//! network access per run, is non-hermetic, and would only be exercising
//! fastembed's own `indicatif` usage — nothing this crate controls.

use std::sync::Arc;

use localdb_core::ingestion::DeletionPolicy;
use localdb_core::{Embedder, IndexJobScope, SourceKind, SourceRow};
use server::JobQueue;

use crate::app_db::{load_config_scaffolded, open_app_db_or_exit};
use crate::cmds::index::{IndexErrorMode, EMBEDDER_BUILD_COUNT_TEST_LOCK};
use crate::cmds::init::run_init_async;
use crate::cmds::search::SearchCmd;
use crate::cmds::store::run_store_add_async;
use crate::cmds::surface::build_mcp_embedder;
use crate::command_table::DaemonAwareCommand;
use crate::daemon_client::CliContext;
use crate::job_attach::run_embedded_store_job;
use crate::progress::DOWNLOAD_PROGRESS_TEST_LOCK;

/// Write a minimal, offline `provider: fake` config rooted at `dir` — the
/// same shape `job_attach::tests::test_config_and_db` and
/// `cmds::source::tests::source_add_across_two_stores_builds_embedder_once`
/// already use.
fn write_fake_config(dir: &std::path::Path) -> std::path::PathBuf {
    let config_path = dir.join("config.yaml");
    std::fs::write(
        &config_path,
        format!(
            "version: 1\npaths:\n  data: {}\ndefaults:\n  indexing:\n    embedding:\n      provider: fake\n      model: bge-small-en-v1.5\n",
            dir.display()
        ),
    )
    .unwrap();
    config_path
}

fn ctx_with(config_path: std::path::PathBuf, json: bool) -> CliContext {
    CliContext {
        config: Some(config_path),
        json,
        stores: vec![],
        yes: false,
        daemon_url: None,
        config_env: None,
    }
}

fn expected_progress(json: bool) -> embed::DownloadProgress {
    if json {
        embed::DownloadProgress::Silent
    } else {
        embed::DownloadProgress::Show
    }
}

/// `init --download-model` is the one call site among the four where
/// `create_embedder` only runs when the caller opts in (`download_model:
/// true`) — the other three build an embedder unconditionally.
#[tokio::test]
async fn run_init_async_threads_download_progress() {
    let _guard = DOWNLOAD_PROGRESS_TEST_LOCK.lock().await;

    for json in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_fake_config(dir.path());
        let ctx = ctx_with(config_path, json);

        embed::reset_last_download_progress();
        run_init_async(&ctx, true).await;

        assert_eq!(
            embed::last_download_progress(),
            Some(expected_progress(json)),
            "run_init_async, json={json}"
        );
    }
}

/// `SearchCmd::run_embedded` only reaches `create_embedder` once its store
/// scope resolves to at least one row — `AllStoresAllowEmpty` returns early
/// on a storeless database (specs/05-surfaces.md §2.2) — so this pre-creates
/// one store before driving the search.
#[tokio::test]
async fn search_run_embedded_threads_download_progress() {
    let _guard = DOWNLOAD_PROGRESS_TEST_LOCK.lock().await;

    for json in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_fake_config(dir.path());

        let setup_ctx = ctx_with(config_path.clone(), false);
        run_store_add_async(&setup_ctx, "docs").await;

        let ctx = ctx_with(config_path, json);
        let config_loader = load_config_scaffolded(&ctx).await;
        let db = open_app_db_or_exit(&ctx, &config_loader).await;

        embed::reset_last_download_progress();
        let cmd = SearchCmd {
            query: "hello",
            limit: 10,
        };
        cmd.run_embedded(&ctx, &config_loader, &db).await.unwrap();

        assert_eq!(
            embed::last_download_progress(),
            Some(expected_progress(json)),
            "SearchCmd::run_embedded, json={json}"
        );
    }
}

/// Drives `build_mcp_embedder` — the embedder-construction step of
/// `localdb mcp`'s embedded mode — rather than `run_mcp_async` as a whole.
///
/// `run_mcp_async` continues into `mcp::serve_embedded_stdio`, whose loop
/// reads the calling process's real stdin through a blocking read. A
/// `tokio::time::timeout` around it drops the future but cannot cancel that
/// read, so whenever stdin is open and idle — an interactive `cargo test`, a
/// CI runner that keeps an input pipe attached — the per-test runtime waits
/// on it at shutdown. Calling the construction step directly removes any
/// dependence on this binary's stdin.
///
/// `run_mcp_async` reaches `create_embedder` only through this function, so
/// the value asserted here is the value that call site threads.
#[tokio::test]
async fn build_mcp_embedder_threads_download_progress() {
    let _guard = DOWNLOAD_PROGRESS_TEST_LOCK.lock().await;

    for json in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_fake_config(dir.path());
        let ctx = ctx_with(config_path, json);
        let config_loader = load_config_scaffolded(&ctx).await;

        embed::reset_last_download_progress();
        let _embedder = build_mcp_embedder(&ctx, &config_loader);

        assert_eq!(
            embed::last_download_progress(),
            Some(expected_progress(json)),
            "build_mcp_embedder, json={json}"
        );
    }
}

/// `run_embedded_store_job` only reaches `create_embedder` when its
/// resolved source scope is non-empty (an unresolvable/empty scope returns
/// before ever building one — see
/// `job_attach::tests::run_embedded_store_job_reports_an_unresolvable_scope_strict_and_warn`),
/// so this pre-creates a store with one source row. The source's root does
/// not need to exist: `WarnAndContinue` swallows the resulting per-source
/// ingestion error into the returned summary, exactly as
/// `job_attach::tests::run_embedded_store_job_warns_and_continues_on_an_invalid_chunker_preset`
/// already relies on — this test only cares that a build happened, not how
/// the job itself concluded.
///
/// This is also the one new test here that builds a real embedder through
/// `job_attach::run_embedded_store_job` — the same call site
/// `cmds::index::EMBEDDER_BUILD_COUNT` counts — so it takes
/// `EMBEDDER_BUILD_COUNT_TEST_LOCK` too (after `DOWNLOAD_PROGRESS_TEST_LOCK`,
/// the consistent order every holder of both locks uses), or its build could
/// interleave into `cmds::source::tests::source_add_across_two_stores_builds_embedder_once`'s
/// single-build measurement window and fail it spuriously.
#[tokio::test]
async fn run_embedded_store_job_threads_download_progress() {
    let _download_guard = DOWNLOAD_PROGRESS_TEST_LOCK.lock().await;
    let _embedder_count_guard = EMBEDDER_BUILD_COUNT_TEST_LOCK.lock().await;

    for json in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_fake_config(dir.path());

        let setup_ctx = ctx_with(config_path.clone(), false);
        let config_loader = load_config_scaffolded(&setup_ctx).await;
        let db = open_app_db_or_exit(&setup_ctx, &config_loader).await;
        run_store_add_async(&setup_ctx, "docs").await;
        let store = db
            .backend()
            .get_store_by_name("docs")
            .await
            .unwrap()
            .unwrap();
        db.backend()
            .upsert_source(&SourceRow {
                id: "01HRQHB7FN3WMX4AZDV3S9VCTZ".to_string(),
                store_id: store.id.clone(),
                kind: SourceKind::Path,
                root: Some("/nonexistent-root".to_string()),
                url: None,
                include: vec![],
                exclude: vec![],
                preset: "prose".to_string(),
                refresh: None,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                config_json: None,
            })
            .await
            .unwrap();

        let ctx = ctx_with(config_path, json);
        let queue = JobQueue::new();
        let mut embedder: Option<Arc<dyn Embedder>> = None;

        embed::reset_last_download_progress();
        run_embedded_store_job(
            &ctx,
            &queue,
            &config_loader,
            &db,
            &store,
            IndexJobScope::Store,
            DeletionPolicy::Retain,
            IndexErrorMode::WarnAndContinue,
            &mut embedder,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            embed::last_download_progress(),
            Some(expected_progress(json)),
            "run_embedded_store_job, json={json}"
        );
    }
}
