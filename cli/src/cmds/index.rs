#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use localdb_core::{
    ingestion::DeletionPolicy, Embedder, Error, IndexJobScope, IndexJobStats, StoreRow,
};
use serde_json::json;
use server::JobQueue;

use crate::{
    app_db::{
        load_app_db, resolve_daemon_store_scope, resolve_store_scope, AppDb, StoreScopePolicy,
    },
    command_table::{dispatch, DaemonAwareCommand},
    daemon_client::CliContext,
    job_attach,
    normalize::{exit_err, print_json},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndexErrorMode {
    StrictExit,
    WarnAndContinue,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IndexSummary {
    has_sources: bool,
    indexed: u64,
    skipped: u64,
    chunks: u64,
    errors: u64,
    unsupported: u64,
    /// Documents no longer present at their source that were kept anyway,
    /// because `--delete` was not passed. Always 0 on a `--delete` run (they
    /// were removed instead).
    prunable: u64,
    /// Documents actually removed (only ever non-zero with `--delete`).
    deleted: u64,
}

impl IndexSummary {
    /// Fold another store's summary into a running total. `has_sources` is
    /// OR-combined: the combined total "has sources" if any contributing
    /// store did.
    fn add(&mut self, other: &IndexSummary) {
        self.has_sources = self.has_sources || other.has_sources;
        self.indexed += other.indexed;
        self.skipped += other.skipped;
        self.chunks += other.chunks;
        self.errors += other.errors;
        self.unsupported += other.unsupported;
        self.prunable += other.prunable;
        self.deleted += other.deleted;
    }

    /// Build an `IndexSummary` from a completed job's `IndexJobStats`
    /// (issue #187 stage 3) — the unified job model's stats shape, produced
    /// identically by the embedded engine (`job_exec::run_job` via a local
    /// `JobQueue`) and a daemon-submitted job alike.
    ///
    /// `has_sources` is derived from `stats.sources_count`, which
    /// `job_exec::run_job` sets to the size of the job's resolved scope
    /// *before* processing anything — 0 only when the scope had nothing to
    /// index at all, distinct from "sources existed but nothing needed
    /// indexing" (`sources_count > 0`, every other counter possibly still 0).
    pub(crate) fn from_job_stats(stats: IndexJobStats) -> Self {
        IndexSummary {
            has_sources: stats.sources_count > 0,
            indexed: stats.docs_indexed,
            skipped: stats.docs_skipped,
            chunks: stats.chunks_written,
            errors: stats.error_count,
            unsupported: stats.unsupported_format_count,
            prunable: stats.docs_prunable,
            deleted: stats.docs_deleted,
        }
    }
}

impl IndexErrorMode {
    pub(crate) fn warn(self) -> bool {
        self == Self::WarnAndContinue
    }
}

/// Test-only construction counter for the `embed::create_embedder` call made
/// by `job_attach::run_embedded_store_job`. Exists purely so the `source add`
/// multi-store auto-index test (`cmds::source::tests`) can assert the
/// embedder is built once across an N-store run, not once per store (Codex
/// review round 2, finding 6). Compiled out entirely in non-test builds.
///
/// Shared per test binary, so it's only safe to assert on because no other
/// test in this crate currently drives `run_embedded_store_job`'s
/// embedder-construction path concurrently; a test reading it resets the
/// counter to 0 immediately before exercising the call it's measuring.
#[cfg(test)]
pub(crate) static EMBEDDER_BUILD_COUNT: AtomicUsize = AtomicUsize::new(0);

/// A single store's index outcome, paired with its name — the unit the
/// summary renderers below combine and format. Kept separate from
/// `IndexSummary` (which has no notion of *which* store it came from) so the
/// single-store and multi-store rendering paths can share one code path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoreIndexOutcome {
    pub(crate) store_name: String,
    pub(crate) summary: IndexSummary,
}

/// `localdb index [--source <id>] [--strict]`
///
/// One-shot scan-and-index (embedded mode) or submits a job to the daemon.
///
/// Per specs/05-surfaces.md §2: when daemon is running, submits job and polls.
/// With `--strict`, exits 2 if any document failed extraction (run always completes).
/// With `--delete`, removes documents that no longer exist at their source;
/// without it nothing is ever removed (see `DeletionPolicy`).
pub fn run_index(ctx: &CliContext, source_id: Option<&str>, strict: bool, delete: bool) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_index_async(ctx, source_id, strict, delete));
}

/// `index`'s table entry (issue #187 stage 5). Both transports were already
/// unified onto the shared async job model in stage 3 — `run_daemon`/
/// `run_embedded` below submit through `job_attach::run_daemon_store_job` /
/// `run_embedded_store_job` respectively and fold the result into the same
/// `Vec<StoreIndexOutcome>` `Outcome`, rendered by the one shared
/// `report_index_outcomes` call in `run_index_async`. Stage 5 only moves the
/// *mode selection* itself onto `command_table::dispatch`, replacing the
/// hand-written `if let DaemonState::Running {...} {...} else {...}` so a
/// future edit can't reintroduce a second, competing `probe_daemon` call.
struct IndexCmd<'a> {
    source_id: Option<&'a str>,
    deletion: DeletionPolicy,
}

impl DaemonAwareCommand for IndexCmd<'_> {
    type Outcome = Vec<StoreIndexOutcome>;

    const SCOPE_POLICY: StoreScopePolicy = StoreScopePolicy::AllStores;

    async fn run_daemon(&self, ctx: &CliContext, base_url: &str) -> Result<Self::Outcome, Error> {
        let store_names = resolve_daemon_store_scope(base_url, ctx, Self::SCOPE_POLICY).await;

        // `--source` narrows the daemon scope to the source's owning
        // store, exactly as `run_embedded` narrows its own `store_rows` —
        // see `resolve_daemon_source_owner`'s doc comment.
        let target_names: Vec<String> = match self.source_id {
            Some(sid) => vec![resolve_daemon_source_owner(base_url, &store_names, sid).await?],
            None => store_names,
        };

        let multi = target_names.len() > 1;
        let mut outcomes = Vec::with_capacity(target_names.len());
        for name in &target_names {
            let label = if multi { Some(name.as_str()) } else { None };
            let summary = match job_attach::run_daemon_store_job(
                ctx,
                base_url,
                name,
                self.source_id,
                self.deletion,
                IndexErrorMode::StrictExit,
                label,
            )
            .await
            {
                Ok(summary) => summary,
                Err(e) => exit_err(&e, ctx.json),
            };
            outcomes.push(StoreIndexOutcome {
                store_name: name.clone(),
                summary,
            });
        }
        Ok(outcomes)
    }

    async fn run_embedded(
        &self,
        ctx: &CliContext,
        config_loader: &localdb_core::config::loader::ConfigLoader,
        db: &AppDb,
    ) -> Result<Self::Outcome, Error> {
        // specs/05-surfaces.md §2.2: `-s` is repeatable and every store
        // scoped by it (or, absent `-s`, every store in the database) is
        // indexed.
        let store_rows = resolve_store_scope(ctx, db, Self::SCOPE_POLICY).await;

        // `--source` names a single, globally-unique source: resolve its
        // owning store once and narrow the run to just that store,
        // rather than passing the same source_id to every store in
        // scope. The latter used to abort the whole run (exit 3,
        // `SourceNotFound`) the moment it reached the first store that
        // *didn't* own the source (#180 review finding 1). An explicit
        // `--store` scope (`ctx.stores` non-empty, reflected in
        // `store_rows` by `resolve_store_scope` above) is a hard filter
        // here: if the source's owner isn't among the
        // explicitly-requested stores, that's still exit 3 — we don't
        // silently redirect to the owner.
        let store_rows: Vec<StoreRow> = if let Some(sid) = self.source_id {
            let owner_store_id = match db.backend().get_source(sid).await? {
                Some(src) => src.store_id,
                None => {
                    return Err(Error::SourceNotFound {
                        id: sid.to_string(),
                    })
                }
            };
            match store_rows.into_iter().find(|r| r.id == owner_store_id) {
                Some(row) => vec![row],
                None => {
                    return Err(Error::SourceNotFound {
                        id: sid.to_string(),
                    })
                }
            }
        } else {
            store_rows
        };

        let multi = store_rows.len() > 1;
        let mut outcomes: Vec<StoreIndexOutcome> = Vec::with_capacity(store_rows.len());
        // Built lazily, on the first store that actually has sources to
        // index — and cached here for the rest of the loop. An empty (or
        // all-empty) scope must not pay for embedder construction, which
        // for the default `local` provider can trigger a one-time
        // ~706 MB model download, just to report "no sources to index"
        // (#180 review finding 2). Once built, it's shared across the
        // remaining stores in scope exactly as before —
        // `job_attach::run_embedded_store_job` threads it in/out of every
        // call, `Some` as soon as it exists regardless of whether that
        // call went on to succeed or fail.
        //
        // This loop uses `IndexErrorMode::StrictExit`, not for embedder
        // caching (the caching holds under either mode — see above), but
        // for `index`'s own semantics: `index` aborts the whole run the
        // moment any store fails (`exit_err` below), unlike `source
        // add`'s auto-index loop (`run_source_add_async`), which
        // deliberately keeps going under `WarnAndContinue` so one bad
        // source doesn't fail the add.
        let mut embedder: Option<Arc<dyn Embedder>> = None;
        let queue = JobQueue::new();
        for store_row in &store_rows {
            let label = if multi {
                Some(store_row.name.as_str())
            } else {
                None
            };
            let scope = match self.source_id {
                Some(sid) => IndexJobScope::Source {
                    source_id: sid.to_string(),
                },
                None => IndexJobScope::Store,
            };
            let summary = match job_attach::run_embedded_store_job(
                ctx,
                &queue,
                config_loader,
                db,
                store_row,
                scope,
                self.deletion,
                IndexErrorMode::StrictExit,
                &mut embedder,
                label,
            )
            .await
            {
                Ok(summary) => summary,
                Err(e) => exit_err(&e, ctx.json),
            };
            outcomes.push(StoreIndexOutcome {
                store_name: store_row.name.clone(),
                summary,
            });
        }
        Ok(outcomes)
    }
}

pub(crate) async fn run_index_async(
    ctx: &CliContext,
    source_id: Option<&str>,
    strict: bool,
    delete: bool,
) {
    let (config_loader, db) = load_app_db(ctx).await;
    let deletion = if delete {
        DeletionPolicy::Prune
    } else {
        DeletionPolicy::Retain
    };

    // D1/D6 (issue #187 stage 3): `--delete` is no longer refused against a
    // daemon (D6) — it is sent as `deletion_policy: "delete"` and the daemon
    // now runs real ingestion (issue #187), so it can honor it.
    let cmd = IndexCmd {
        source_id,
        deletion,
    };
    let outcomes = dispatch(&cmd, ctx, &config_loader, &db).await;

    report_index_outcomes(ctx, &outcomes, strict);
}

/// Walk `GET {base_url}/v1/stores/{store_name}/sources`, paginating to
/// exhaustion, to check whether `store_name` owns `source_id`.
///
/// Used by `resolve_daemon_source_owner` below. Must paginate: `PaginatedList`
/// truncates each page to `default_limit()` (20), so a single unpaginated
/// fetch would silently miss a source sitting on page 2+ — turning the
/// finding-2 fix into a worse bug than the one it replaces.
///
/// `store_name` is percent-encoded via `encode_path_segment` before it's
/// interpolated into the URL path — an unescaped `#`/`?`/`/` would otherwise
/// retarget the request at a different endpoint entirely (finding 1). Page
/// walking, the malformed-shape check, and the pagination-cycle guard are
/// shared with `fetch_all_daemon_store_names` (`cli/src/app_db.rs`) via
/// `daemon_client::walk_daemon_pages` — see its doc comment. In particular, a
/// response with no (or non-array) `items` field is an error here, not a
/// silent "source not found": that swallow was exactly how a request that
/// silently landed on the wrong endpoint (finding 1's bug) used to be
/// misreported as a clean "not found" instead of failing loudly.
async fn daemon_store_has_source(
    base_url: &str,
    store_name: &str,
    source_id: &str,
) -> Result<bool, Error> {
    let path = format!(
        "/v1/stores/{}/sources",
        crate::daemon_client::encode_path_segment(store_name)
    );
    let mut found = false;
    crate::daemon_client::walk_daemon_pages(base_url, &path, |items| {
        if items
            .iter()
            .any(|it| it.get("id").and_then(|i| i.as_str()) == Some(source_id))
        {
            found = true;
            true
        } else {
            false
        }
    })
    .await?;
    Ok(found)
}

/// Narrow a daemon scope (of any size, including a single store — see
/// `run_index_async`'s daemon branch, finding 4) down to `source_id`'s
/// owning store (Codex review round 2, finding 2).
///
/// `/v1/jobs` (`server/src/handlers/jobs.rs`'s `create_job`) validates only
/// `store_name` — `source_id` is checked neither for existence nor for
/// ownership — so without this, submitting the same `source_id` to every
/// store in `scoped_names` would silently accept a job for every one of them,
/// only one of which is meaningful, and a single-store scope would submit
/// with zero verification at all. Walks the scoped stores in order via
/// `daemon_store_has_source`, returning the first owner found.
///
/// Not found in any scoped store is `Error::SourceNotFound`, exit 3 — the
/// same outcome an explicit `--store` scope that excludes the true owner
/// produces, reproducing the embedded path's hard-filter rule for free (see
/// `index_source_owner_not_in_explicit_store_scope_exits_3`).
async fn resolve_daemon_source_owner(
    base_url: &str,
    scoped_names: &[String],
    source_id: &str,
) -> Result<String, Error> {
    for name in scoped_names {
        if daemon_store_has_source(base_url, name, source_id).await? {
            return Ok(name.clone());
        }
    }
    Err(Error::SourceNotFound {
        id: source_id.to_string(),
    })
}

/// Sum every store's summary into a single combined total. `has_sources` is
/// true if any contributing store had sources.
pub(crate) fn total_summary(outcomes: &[StoreIndexOutcome]) -> IndexSummary {
    let mut total = IndexSummary::default();
    for outcome in outcomes {
        total.add(&outcome.summary);
    }
    total
}

/// Whether `--strict` should force a nonzero exit for this outcome set.
/// specs/05-surfaces.md §2.2/§5: `--strict` exits 2 if *any* store reported
/// errors, but only after every store has finished running.
pub(crate) fn strict_should_fail(outcomes: &[StoreIndexOutcome], strict: bool) -> bool {
    strict && outcomes.iter().any(|o| o.summary.errors > 0)
}

fn summary_status(summary: &IndexSummary, strict: bool) -> &'static str {
    if strict && summary.errors > 0 {
        "error"
    } else {
        "ok"
    }
}

/// Render the "N indexed, N skipped, ..." body shared by the single-store
/// line, each multi-store line, and the combined total line.
fn format_summary_body(summary: &IndexSummary) -> String {
    let mut body = format!(
        "{} indexed, {} skipped, {} chunks written, {} unsupported, {} errors",
        summary.indexed, summary.skipped, summary.chunks, summary.unsupported, summary.errors
    );
    // Only ever one of these is non-zero: `prunable` counts what a retaining
    // run kept, `deleted` what a `--delete` run removed.
    if summary.deleted > 0 {
        body.push_str(&format!(", {} deleted", summary.deleted));
    }
    if summary.prunable > 0 {
        body.push_str(&format!(
            ", {} no longer at source (kept; use --delete to remove)",
            summary.prunable
        ));
    }
    body
}

/// Render the full text report for a set of store outcomes.
///
/// A single outcome renders exactly as the pre-multi-store format did (no
/// store-name prefix, no total line) so existing scripts/output don't break.
/// More than one outcome gets a `[store]` prefix per line plus a trailing
/// `Total:` line. Pure function — unit-tested directly below.
pub(crate) fn render_index_text(outcomes: &[StoreIndexOutcome]) -> String {
    let multi = outcomes.len() > 1;
    let mut lines = Vec::with_capacity(outcomes.len() + 1);
    for outcome in outcomes {
        if !outcome.summary.has_sources {
            lines.push(if multi {
                format!("[{}] No sources to index.", outcome.store_name)
            } else {
                format!("No sources to index on store '{}'.", outcome.store_name)
            });
            continue;
        }
        let body = format_summary_body(&outcome.summary);
        lines.push(if multi {
            format!("[{}] Index complete: {}", outcome.store_name, body)
        } else {
            format!("Index complete: {}", body)
        });
    }
    if multi {
        lines.push(format!(
            "Total: {}",
            format_summary_body(&total_summary(outcomes))
        ));
    }
    lines.join("\n")
}

fn summary_fields_json(summary: &IndexSummary, strict: bool) -> serde_json::Value {
    if !summary.has_sources {
        return json!({ "status": "ok", "message": "no sources to index" });
    }
    json!({
        "status": summary_status(summary, strict),
        "docs_indexed": summary.indexed,
        "docs_skipped": summary.skipped,
        "chunks_written": summary.chunks,
        "unsupported": summary.unsupported,
        "errors": summary.errors,
        "docs_deleted": summary.deleted,
        "docs_prunable": summary.prunable,
    })
}

/// Render the JSON report for a set of store outcomes.
///
/// A single outcome renders as the exact pre-existing flat object (no
/// wrapping, no `store` field) so `--json` output for the single-store case
/// is unchanged. More than one outcome wraps into `{"stores": [...], "total":
/// {...}}`, each store entry carrying a `store` name field. Pure function —
/// unit-tested directly below.
pub(crate) fn render_index_json(outcomes: &[StoreIndexOutcome], strict: bool) -> serde_json::Value {
    if let [only] = outcomes {
        return summary_fields_json(&only.summary, strict);
    }
    let stores: Vec<serde_json::Value> = outcomes
        .iter()
        .map(|o| {
            let mut fields = summary_fields_json(&o.summary, strict);
            if let Some(obj) = fields.as_object_mut() {
                obj.insert("store".to_string(), json!(o.store_name));
            }
            fields
        })
        .collect();
    json!({
        "stores": stores,
        "total": summary_fields_json(&total_summary(outcomes), strict),
    })
}

fn report_index_outcomes(ctx: &CliContext, outcomes: &[StoreIndexOutcome], strict: bool) {
    if ctx.json {
        print_json(&render_index_json(outcomes, strict));
    } else {
        println!("{}", render_index_text(outcomes));
    }
    if strict_should_fail(outcomes, strict) {
        std::process::exit(2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(name: &str, summary: IndexSummary) -> StoreIndexOutcome {
        StoreIndexOutcome {
            store_name: name.to_string(),
            summary,
        }
    }

    fn with_sources(
        indexed: u64,
        skipped: u64,
        chunks: u64,
        unsupported: u64,
        errors: u64,
    ) -> IndexSummary {
        IndexSummary {
            has_sources: true,
            indexed,
            skipped,
            chunks,
            errors,
            unsupported,
            prunable: 0,
            deleted: 0,
        }
    }

    // -- total_summary --------------------------------------------------

    #[test]
    fn total_summary_sums_fields_across_stores() {
        let outcomes = vec![
            outcome("a", with_sources(3, 1, 6, 0, 0)),
            outcome("b", with_sources(1, 0, 2, 1, 2)),
        ];
        let total = total_summary(&outcomes);
        assert_eq!(total.indexed, 4);
        assert_eq!(total.skipped, 1);
        assert_eq!(total.chunks, 8);
        assert_eq!(total.unsupported, 1);
        assert_eq!(total.errors, 2);
        assert!(total.has_sources);
    }

    #[test]
    fn total_summary_has_sources_false_when_no_store_has_sources() {
        let outcomes = vec![
            outcome("a", IndexSummary::default()),
            outcome("b", IndexSummary::default()),
        ];
        assert!(!total_summary(&outcomes).has_sources);
    }

    #[test]
    fn total_summary_has_sources_true_when_any_store_has_sources() {
        let outcomes = vec![
            outcome("a", IndexSummary::default()),
            outcome("b", with_sources(1, 0, 1, 0, 0)),
        ];
        assert!(total_summary(&outcomes).has_sources);
    }

    #[test]
    fn total_summary_empty_outcomes_is_default() {
        assert_eq!(total_summary(&[]), IndexSummary::default());
    }

    // -- strict_should_fail -----------------------------------------------

    #[test]
    fn strict_should_fail_false_without_strict_flag() {
        let outcomes = vec![outcome("a", with_sources(0, 0, 0, 0, 5))];
        assert!(!strict_should_fail(&outcomes, false));
    }

    #[test]
    fn strict_should_fail_false_with_strict_flag_and_no_errors() {
        let outcomes = vec![outcome("a", with_sources(3, 0, 6, 0, 0))];
        assert!(!strict_should_fail(&outcomes, true));
    }

    #[test]
    fn strict_should_fail_true_when_any_store_errored() {
        let outcomes = vec![
            outcome("a", with_sources(3, 0, 6, 0, 0)),
            outcome("b", with_sources(1, 0, 2, 0, 1)),
        ];
        assert!(strict_should_fail(&outcomes, true));
    }

    // -- render_index_text --------------------------------------------------

    #[test]
    fn render_index_text_single_store_matches_legacy_format() {
        let outcomes = vec![outcome("books", with_sources(3, 1, 6, 0, 0))];
        assert_eq!(
            render_index_text(&outcomes),
            "Index complete: 3 indexed, 1 skipped, 6 chunks written, 0 unsupported, 0 errors"
        );
    }

    #[test]
    fn render_index_text_single_store_no_sources_matches_legacy_format() {
        let outcomes = vec![outcome("books", IndexSummary::default())];
        assert_eq!(
            render_index_text(&outcomes),
            "No sources to index on store 'books'."
        );
    }

    #[test]
    fn render_index_text_multi_store_prefixes_and_appends_total() {
        let outcomes = vec![
            outcome("books", with_sources(3, 1, 6, 0, 0)),
            outcome("notes", IndexSummary::default()),
        ];
        let rendered = render_index_text(&outcomes);
        assert_eq!(
            rendered,
            "[books] Index complete: 3 indexed, 1 skipped, 6 chunks written, 0 unsupported, 0 errors\n\
             [notes] No sources to index.\n\
             Total: 3 indexed, 1 skipped, 6 chunks written, 0 unsupported, 0 errors"
        );
    }

    // -- render_index_json --------------------------------------------------

    #[test]
    fn render_index_json_single_store_matches_legacy_flat_shape() {
        let outcomes = vec![outcome("books", with_sources(3, 1, 6, 0, 0))];
        let v = render_index_json(&outcomes, false);
        assert_eq!(
            v,
            json!({
                "status": "ok",
                "docs_indexed": 3,
                "docs_skipped": 1,
                "chunks_written": 6,
                "unsupported": 0,
                "errors": 0,
                // Added alongside the opt-in `--delete` flag: a retaining run
                // has to be able to tell consumers what pruning would remove.
                "docs_deleted": 0,
                "docs_prunable": 0,
            })
        );
        assert!(
            v.get("store").is_none(),
            "single-store JSON must not gain a store field"
        );
    }

    #[test]
    fn render_index_json_single_store_no_sources_matches_legacy_shape() {
        let outcomes = vec![outcome("books", IndexSummary::default())];
        let v = render_index_json(&outcomes, false);
        assert_eq!(
            v,
            json!({ "status": "ok", "message": "no sources to index" })
        );
    }

    #[test]
    fn render_index_json_multi_store_wraps_with_total() {
        let outcomes = vec![
            outcome("books", with_sources(3, 1, 6, 0, 0)),
            outcome("notes", with_sources(1, 0, 2, 0, 1)),
        ];
        let v = render_index_json(&outcomes, false);
        assert_eq!(
            v,
            json!({
                "stores": [
                    {
                        "store": "books",
                        "status": "ok",
                        "docs_indexed": 3,
                        "docs_skipped": 1,
                        "chunks_written": 6,
                        "unsupported": 0,
                        "errors": 0,
                        "docs_deleted": 0,
                        "docs_prunable": 0,
                    },
                    {
                        "store": "notes",
                        "status": "ok",
                        "docs_indexed": 1,
                        "docs_skipped": 0,
                        "chunks_written": 2,
                        "unsupported": 0,
                        "errors": 1,
                        "docs_deleted": 0,
                        "docs_prunable": 0,
                    },
                ],
                "total": {
                    "status": "ok",
                    "docs_indexed": 4,
                    "docs_skipped": 1,
                    "chunks_written": 8,
                    "unsupported": 0,
                    "errors": 1,
                    "docs_deleted": 0,
                    "docs_prunable": 0,
                },
            })
        );
    }

    #[test]
    fn render_index_json_strict_marks_errored_stores_and_total() {
        let outcomes = vec![
            outcome("books", with_sources(3, 0, 6, 0, 0)),
            outcome("notes", with_sources(1, 0, 2, 0, 1)),
        ];
        let v = render_index_json(&outcomes, true);
        assert_eq!(v["stores"][0]["status"], "ok");
        assert_eq!(v["stores"][1]["status"], "error");
        assert_eq!(v["total"]["status"], "error");
    }

    #[test]
    fn render_index_json_multi_store_all_without_sources() {
        let outcomes = vec![
            outcome("books", IndexSummary::default()),
            outcome("notes", IndexSummary::default()),
        ];
        let v = render_index_json(&outcomes, false);
        assert_eq!(
            v["stores"][0],
            json!({ "store": "books", "status": "ok", "message": "no sources to index" })
        );
        assert_eq!(
            v["total"],
            json!({ "status": "ok", "message": "no sources to index" })
        );
    }
}
