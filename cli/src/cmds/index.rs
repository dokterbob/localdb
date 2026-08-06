use std::sync::Arc;

use fetch::HttpUrlFetcher;
use localdb_core::{
    config::{loader::ConfigLoader, policy::compute_policy_version},
    Embedder, Error, SourceRow, StoreRow,
};
use serde_json::json;

use crate::{
    app_db::{load_app_db, resolve_store_scope, AppDb, StoreScopePolicy},
    daemon_client::{daemon_request_async, probe_daemon, CliContext, DaemonState},
    normalize::{exit_err, print_json, source_row_to_core_source},
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
    }
}

impl IndexErrorMode {
    fn warn(self) -> bool {
        self == Self::WarnAndContinue
    }
}

/// A single store's index outcome, paired with its name — the unit the
/// summary renderers below combine and format. Kept separate from
/// `IndexSummary` (which has no notion of *which* store it came from) so the
/// single-store and multi-store rendering paths can share one code path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoreIndexOutcome {
    pub(crate) store_name: String,
    pub(crate) summary: IndexSummary,
}

/// Index one store, using an already-open `AppDb`/`ConfigLoader` and
/// (optionally) an already-built embedder.
///
/// Callers that already hold an open `AppDb` and want to index several
/// stores in one run (`run_index_async`) should call this directly and pass
/// `embedder` through from store to store: `None` until one is built, then
/// `Some(..)` for the rest — reloading a ~706 MB local embedding model per
/// store would be wasteful. The `run_embedded_index` wrapper below is for
/// callers with only a `CliContext` and a single store in hand (e.g. the
/// post-`source add` auto-index).
///
/// Returns the summary alongside the embedder actually used (`Some` once
/// this store's sources required constructing or reusing one, `None` when
/// the store had no sources to index and no embedder was touched at all —
/// this is what lets a multi-store run skip building the embedder entirely
/// when every store in scope is empty). A caller looping over stores should
/// carry the returned embedder forward into the next call.
///
/// `progress_label` is rendered as a `[label]` prefix on progress output when
/// `Some` — set it only when more than one store is in scope for the run, so
/// single-store output is unchanged.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_embedded_index_with(
    ctx: &CliContext,
    db: &AppDb,
    config_loader: &ConfigLoader,
    store_row: &StoreRow,
    source_id: Option<&str>,
    mode: IndexErrorMode,
    embedder: Option<Arc<dyn Embedder>>,
    progress_label: Option<&str>,
) -> Result<(IndexSummary, Option<Arc<dyn Embedder>>), Error> {
    use localdb_core::{
        chunker::ChunkerConfig,
        ingestion::{run_source_ingestion, DocumentIndex, IngestionConfig, SourceIngestionDeps},
        ingestor::Ingestor,
        types::SourceSpec,
    };

    macro_rules! warn_or_default {
        ($expr:expr, $fmt:literal) => {
            match $expr {
                Ok(value) => value,
                Err(e) => {
                    let error = Error::from(e);
                    if mode.warn() {
                        eprintln!($fmt, error);
                        return Ok((IndexSummary::default(), None));
                    }
                    return Err(error);
                }
            }
        };
    }

    let all_sources = warn_or_default!(
        db.backend().list_sources(&store_row.id).await,
        "warning: cannot list sources for auto-index: {}"
    );

    let sources_to_index: Vec<SourceRow> = if let Some(sid) = source_id {
        match all_sources.into_iter().find(|s| s.id == sid) {
            Some(s) => vec![s],
            None if mode.warn() => return Ok((IndexSummary::default(), None)),
            None => {
                return Err(Error::SourceNotFound {
                    id: sid.to_string(),
                })
            }
        }
    } else {
        all_sources
    };

    if sources_to_index.is_empty() {
        return Ok((IndexSummary::default(), None));
    }

    let policy = config_loader.config.defaults.indexing.clone();
    let current_policy_version = compute_policy_version(&config_loader.config.defaults.indexing);
    if store_row.policy_version != current_policy_version {
        let new_indexing_policy =
            serde_json::to_string(&policy).unwrap_or_else(|_| store_row.indexing_policy.clone());
        let updated_store = StoreRow {
            policy_version: current_policy_version.clone(),
            indexing_policy: new_indexing_policy,
            ..store_row.clone()
        };
        if let Err(e) = db.backend().upsert_store(&updated_store).await {
            eprintln!("warning: failed to update policy_version: {}", e);
        }
    }
    let ingestion_cfg = IngestionConfig {
        store_id: store_row.id.clone(),
        policy_version: current_policy_version,
        chunker: ChunkerConfig::prose(),
    };

    let embedder: Arc<dyn Embedder> = if let Some(embedder) = embedder {
        embedder
    } else {
        let built = warn_or_default!(
            embed::create_embedder(
                &config_loader.config.defaults.indexing.embedding,
                &config_loader.config.providers,
                Some(&config_loader.paths.models_dir),
            ),
            "warning: cannot create embedder for auto-index: {}"
        );
        Arc::from(built)
    };
    // Validate the parser chain once up front — fail-fast parity with the
    // legacy path, which built its single extractor before the loop. Parser
    // instances are cheap unit structs (not `Clone`), so each source below
    // rebuilds its own owned chain from the same `policy.parsers` ids rather
    // than sharing this one.
    warn_or_default!(
        extract::build_chain(&policy.parsers),
        "warning: cannot build parser chain for auto-index: {}"
    );
    let handle = warn_or_default!(
        db.backend().retrieval_store(&store_row.id).await,
        "warning: cannot open store handle for auto-index: {}"
    );
    let existing = warn_or_default!(
        handle.list_indexed_documents().await,
        "warning: cannot read existing documents for auto-index: {}"
    );
    let mut doc_index = DocumentIndex::from_records(existing);
    let url_fetcher = HttpUrlFetcher::new()?;
    let mut summary = IndexSummary {
        has_sources: true,
        ..IndexSummary::default()
    };

    for rt_source in &sources_to_index {
        let source = source_row_to_core_source(rt_source);
        let chunker = match ChunkerConfig::from_preset(&source.source_preset) {
            Ok(chunker) => chunker,
            Err(e) => {
                summary.errors += 1;
                if mode.warn() {
                    eprintln!(
                        "warning: invalid chunker preset '{}' for source {}: {}",
                        source.source_preset, rt_source.id, e
                    );
                } else {
                    eprintln!(
                        "error indexing source {}: invalid chunker preset '{}': {}",
                        rt_source.id, source.source_preset, e
                    );
                }
                continue;
            }
        };
        let cfg = IngestionConfig {
            chunker,
            ..ingestion_cfg.clone()
        };
        let sink = crate::progress::build_progress_sink(ctx.json, progress_label);

        // Build the concrete `Ingestor` for this source's kind — the CLI is
        // the composition root that wires I/O-owning `ingest` crate types
        // into the I/O-free `core` pipeline (specs/01-architecture.md §1).
        let parser_chain =
            extract::build_chain(&policy.parsers).expect("parser chain already validated above");
        let ingestor: Box<dyn Ingestor> = match &source.spec {
            SourceSpec::Path { .. } => Box::new(ingest::FileIngestor::new(Box::new(parser_chain))),
            SourceSpec::Url { .. } => Box::new(ingest::UrlIngestor::new(
                Box::new(parser_chain),
                Box::new(url_fetcher.clone()),
            )),
        };

        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: handle.as_ref(),
            embedder: embedder.as_ref(),
            config: &cfg,
            progress: sink,
        };

        match run_source_ingestion(&source, ingestor.as_ref(), deps).await {
            Ok(r) => {
                summary.indexed += r.docs_indexed;
                summary.skipped += r.docs_skipped;
                summary.chunks += r.chunks_written;
                summary.errors += r.error_count;
                summary.unsupported += r.unsupported_format_count;
            }
            Err(e) => {
                summary.errors += 1;
                if mode.warn() {
                    eprintln!(
                        "warning: auto-index error for source {}: {}",
                        rt_source.id, e
                    );
                } else {
                    eprintln!("error indexing source {}: {}", rt_source.id, e);
                }
            }
        }
    }

    Ok((summary, Some(embedder)))
}

/// Index one store, opening its own `AppDb` and building its own embedder.
///
/// **Signature is a hard constraint**: `source.rs`'s post-`source add`
/// auto-index call site depends on this exact shape. Multi-store callers
/// (`run_index_async`) should call `run_embedded_index_with` directly instead
/// — it accepts an already-open `AppDb` and a pre-built embedder so an N-store
/// run doesn't reopen the DB or reload the embedding model N times.
pub(crate) async fn run_embedded_index(
    ctx: &CliContext,
    store_row: &StoreRow,
    source_id: Option<&str>,
    mode: IndexErrorMode,
) -> Result<IndexSummary, Error> {
    let (config_loader, db) = load_app_db(ctx).await;
    let (summary, _embedder) = run_embedded_index_with(
        ctx,
        &db,
        &config_loader,
        store_row,
        source_id,
        mode,
        None,
        None,
    )
    .await?;
    Ok(summary)
}

/// `localdb index [--source <id>] [--strict]`
///
/// One-shot scan-and-index (embedded mode) or submits a job to the daemon.
///
/// Per specs/05-surfaces.md §2: when daemon is running, submits job and polls.
/// With `--strict`, exits 2 if any document failed extraction (run always completes).
pub fn run_index(ctx: &CliContext, source_id: Option<&str>, strict: bool) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_index_async(ctx, source_id, strict));
}

pub(crate) async fn run_index_async(ctx: &CliContext, source_id: Option<&str>, strict: bool) {
    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = config_loader.paths.data_dir.clone();

    // specs/05-surfaces.md §2.2: `-s` is repeatable and every store scoped by
    // it (or, absent `-s`, every store in the database) is indexed.
    let store_rows = resolve_store_scope(ctx, &db, StoreScopePolicy::AllStores).await;

    // Per specs/05-surfaces.md §2: when daemon is running, submit a job per
    // resolved store instead of indexing embedded. `/v1/jobs` is a
    // single-store API (server/src/handlers/jobs.rs), so a multi-store scope
    // becomes one POST per store here rather than a batched request. The
    // daemon — not this process — validates `--source`, so no local source
    // resolution (below) applies on this path.
    if let DaemonState::Running { base_url } = probe_daemon(&data_dir, ctx.daemon_url.as_deref()) {
        run_daemon_index(ctx, &base_url, &store_rows, source_id).await;
        return;
    }

    // `--source` names a single, globally-unique source: resolve its owning
    // store once and narrow the run to just that store, rather than passing
    // the same source_id to every store in scope. The latter used to abort
    // the whole run (exit 3, `SourceNotFound`) the moment it reached the
    // first store that *didn't* own the source (#180 review finding 1). An
    // explicit `--store` scope (`ctx.stores` non-empty, reflected in
    // `store_rows` by `resolve_store_scope` above) is a hard filter here: if
    // the source's owner isn't among the explicitly-requested stores, that's
    // still exit 3 — we don't silently redirect to the owner.
    let store_rows: Vec<StoreRow> = if let Some(sid) = source_id {
        let owner_store_id = match db.backend().get_source(sid).await {
            Ok(Some(src)) => src.store_id,
            Ok(None) => exit_err(
                &Error::SourceNotFound {
                    id: sid.to_string(),
                },
                ctx.json,
            ),
            Err(e) => exit_err(&e, ctx.json),
        };
        match store_rows.into_iter().find(|r| r.id == owner_store_id) {
            Some(row) => vec![row],
            None => exit_err(
                &Error::SourceNotFound {
                    id: sid.to_string(),
                },
                ctx.json,
            ),
        }
    } else {
        store_rows
    };

    let multi = store_rows.len() > 1;
    let mut outcomes: Vec<StoreIndexOutcome> = Vec::with_capacity(store_rows.len());
    // Built lazily, on the first store that actually has sources to index —
    // and cached here for the rest of the loop. An empty (or all-empty)
    // scope must not pay for embedder construction, which for the default
    // `local` provider can trigger a one-time ~706 MB model download, just
    // to report "no sources to index" (#180 review finding 2). Once built,
    // it's shared across the remaining stores in scope exactly as before —
    // an N-store run still constructs the embedder at most once.
    //
    // INVARIANT: this loop must stay on `IndexErrorMode::StrictExit`. Under
    // `WarnAndContinue`, a store that fails *after* building the embedder
    // (e.g. `retrieval_store` or `list_indexed_documents` erroring) returns
    // `None` for `used_embedder`, so the next store would rebuild it —
    // reintroducing the per-store ~706 MB reload this cache exists to avoid.
    // `StrictExit` makes that unreachable: the same failure returns `Err` and
    // exits below, so there is no "next store". If you ever want `index` to
    // continue past a failing store, thread the embedder out of that path too
    // rather than just switching the mode.
    let mut embedder: Option<Arc<dyn Embedder>> = None;
    for store_row in &store_rows {
        let label = if multi {
            Some(store_row.name.as_str())
        } else {
            None
        };
        let (summary, used_embedder) = match run_embedded_index_with(
            ctx,
            &db,
            &config_loader,
            store_row,
            source_id,
            IndexErrorMode::StrictExit,
            embedder.clone(),
            label,
        )
        .await
        {
            Ok(result) => result,
            Err(e) => exit_err(&e, ctx.json),
        };
        if embedder.is_none() {
            embedder = used_embedder;
        }
        outcomes.push(StoreIndexOutcome {
            store_name: store_row.name.clone(),
            summary,
        });
    }

    report_index_outcomes(ctx, &outcomes, strict);
}

/// Submit one `/v1/jobs` request per resolved store to a running daemon and
/// report the submissions. `/v1/jobs` is single-store only
/// (`server/src/handlers/jobs.rs`'s `CreateJobRequest`), so there is no
/// batched request to make here — this is intentionally simple submit-and-
/// report, matching what the single-store path already did, looped.
///
/// A submission failure exits immediately (via `exit_err`), the same as the
/// pre-existing single-store behavior: unlike the embedded-index loop, a
/// failed *submission* hasn't done any indexing work yet, so there's nothing
/// gained by continuing to submit further stores' jobs after one enqueue call
/// fails — that's a distinct concern from `--strict`'s "never abort mid-run",
/// which is about not discarding embedded-mode work already done.
async fn run_daemon_index(
    ctx: &CliContext,
    base_url: &str,
    store_rows: &[StoreRow],
    source_id: Option<&str>,
) {
    let mut submissions: Vec<(String, serde_json::Value)> = Vec::with_capacity(store_rows.len());
    for store_row in store_rows {
        let url = format!("{}/v1/jobs", base_url);
        let mut body = json!({ "store_name": store_row.name });
        if let Some(sid) = source_id {
            body["source_id"] = serde_json::Value::String(sid.to_string());
        }
        match daemon_request_async(reqwest::Method::POST, &url, Some(body)).await {
            Ok(v) => submissions.push((store_row.name.clone(), v)),
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    if ctx.json {
        if let [(_, only)] = submissions.as_slice() {
            print_json(only);
        } else {
            let jobs: Vec<serde_json::Value> = submissions
                .into_iter()
                .map(|(name, mut v)| {
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("store".to_string(), json!(name));
                    }
                    v
                })
                .collect();
            print_json(&json!({ "jobs": jobs }));
        }
    } else {
        let multi = submissions.len() > 1;
        for (name, v) in &submissions {
            let job_id = v.get("id").and_then(|i| i.as_str()).unwrap_or("?");
            if multi {
                println!(
                    "Index job submitted to daemon for store '{}': {} (poll with status)",
                    name, job_id
                );
            } else {
                println!(
                    "Index job submitted to daemon: {} (poll with status)",
                    job_id
                );
            }
        }
    }
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
    format!(
        "{} indexed, {} skipped, {} chunks written, {} unsupported, {} errors",
        summary.indexed, summary.skipped, summary.chunks, summary.unsupported, summary.errors
    )
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
                    },
                    {
                        "store": "notes",
                        "status": "ok",
                        "docs_indexed": 1,
                        "docs_skipped": 0,
                        "chunks_written": 2,
                        "unsupported": 0,
                        "errors": 1,
                    },
                ],
                "total": {
                    "status": "ok",
                    "docs_indexed": 4,
                    "docs_skipped": 1,
                    "chunks_written": 8,
                    "unsupported": 0,
                    "errors": 1,
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
