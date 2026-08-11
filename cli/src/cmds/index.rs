#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use fetch::HttpUrlFetcher;
use localdb_core::{
    config::{loader::ConfigLoader, policy::compute_policy_version},
    Embedder, Error, SourceRow, StoreRow,
};
use serde_json::json;

use crate::{
    app_db::{
        load_app_db, resolve_daemon_store_scope, resolve_store_scope, AppDb, StoreScopePolicy,
    },
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
}

impl IndexErrorMode {
    fn warn(self) -> bool {
        self == Self::WarnAndContinue
    }
}

/// Test-only construction counter for the `embed::create_embedder` call made
/// by `run_embedded_index_with` below. Exists purely so the `source add`
/// multi-store auto-index test (`cmds::source::tests`) can assert the
/// embedder is built once across an N-store run, not once per store (Codex
/// review round 2, finding 6). Compiled out entirely in non-test builds.
///
/// Shared per test binary, so it's only safe to assert on because no other
/// test in this crate currently drives `run_embedded_index_with`'s
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

/// Index one store, using an already-open `AppDb`/`ConfigLoader` and
/// (optionally) an already-built embedder.
///
/// Every multi-store caller — `run_index_async` and `run_source_add_async`'s
/// post-`source add` auto-index loop alike — calls this directly and threads
/// `embedder` through from store to store: `None` until one is built, then
/// `Some(..)` for the rest — reloading a ~706 MB local embedding model per
/// store would be wasteful.
///
/// Returns the summary alongside the embedder actually used. This is `Some`
/// as soon as this call has built or reused an embedder — including when the
/// call goes on to fail *after* that point (e.g. `retrieval_store` or
/// `list_indexed_documents` erroring under `IndexErrorMode::WarnAndContinue`)
/// — so a caller looping under `WarnAndContinue` keeps the cache even after a
/// mid-store failure. It's `None` only when no sources needed indexing, or a
/// failure happened before the embedder was ever touched; this is what lets a
/// multi-store run skip building the embedder entirely when every store in
/// scope is empty. A caller looping over stores should carry the returned
/// embedder forward into the next call.
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
    deletion: localdb_core::ingestion::DeletionPolicy,
) -> Result<(IndexSummary, Option<Arc<dyn Embedder>>), Error> {
    use localdb_core::{
        chunker::ChunkerConfig,
        ingestion::{run_source_ingestion, DocumentIndex, IngestionConfig, SourceIngestionDeps},
        ingestor::Ingestor,
        types::SourceSpec,
    };

    macro_rules! warn_or_default {
        ($expr:expr, $fmt:literal) => {
            warn_or_default!($expr, $fmt, None)
        };
        // Three-arg form: `$embedder_on_warn` is what to report as the used
        // embedder when this failure is warned-and-swallowed. Callers after
        // the embedder is built pass `Some(embedder.clone())` so a mid-store
        // failure under `IndexErrorMode::WarnAndContinue` doesn't discard an
        // embedder that was actually constructed (see the doc comment above).
        ($expr:expr, $fmt:literal, $embedder_on_warn:expr) => {
            match $expr {
                Ok(value) => value,
                Err(e) => {
                    let error = Error::from(e);
                    if mode.warn() {
                        eprintln!($fmt, error);
                        return Ok((IndexSummary::default(), $embedder_on_warn));
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
        #[cfg(test)]
        EMBEDDER_BUILD_COUNT.fetch_add(1, Ordering::SeqCst);
        Arc::from(built)
    };
    // Validate the parser chain once up front — fail-fast parity with the
    // legacy path, which built its single extractor before the loop. Parser
    // instances are cheap unit structs (not `Clone`), so each source below
    // rebuilds its own owned chain from the same `policy.parsers` ids rather
    // than sharing this one.
    //
    // From here on, `embedder` is already built/reused, so every
    // `warn_or_default!` call below passes `Some(embedder.clone())` as what
    // to report on a swallowed failure — see the doc comment above.
    warn_or_default!(
        extract::build_chain(&policy.parsers),
        "warning: cannot build parser chain for auto-index: {}",
        Some(embedder.clone())
    );
    let handle = warn_or_default!(
        db.backend().retrieval_store(&store_row.id).await,
        "warning: cannot open store handle for auto-index: {}",
        Some(embedder.clone())
    );
    let existing = warn_or_default!(
        handle.list_indexed_documents().await,
        "warning: cannot read existing documents for auto-index: {}",
        Some(embedder.clone())
    );
    let mut doc_index = DocumentIndex::from_records(existing);
    let url_fetcher = warn_or_default!(
        HttpUrlFetcher::new(),
        "warning: cannot build HTTP client for auto-index: {}",
        Some(embedder.clone())
    );
    // Second client, for locators that come from *content* rather than from
    // the operator: today only a feed entry's `<link>`. It refuses any
    // destination that is not globally routable, so a hostile feed cannot
    // steer localdb at `169.254.169.254` or a LAN admin panel and have the
    // response indexed into a searchable store. The feed's own URL keeps the
    // unrestricted client above — it is operator-typed, the same trust class
    // as a `url` source. See `ingest::FeedIngestor::new`.
    let entry_fetcher = warn_or_default!(
        HttpUrlFetcher::new_public_only(),
        "warning: cannot build public-only HTTP client for auto-index: {}",
        Some(embedder.clone())
    );
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
            SourceSpec::Feed { .. } => Box::new(ingest::FeedIngestor::new(
                Box::new(parser_chain),
                Box::new(url_fetcher.clone()),
                Box::new(entry_fetcher.clone()),
            )),
        };

        let deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: handle.as_ref(),
            embedder: embedder.as_ref(),
            config: &cfg,
            progress: sink,
            deletion,
        };

        match run_source_ingestion(&source, ingestor.as_ref(), deps).await {
            Ok(r) => {
                summary.indexed += r.docs_indexed;
                summary.skipped += r.docs_skipped;
                summary.chunks += r.chunks_written;
                summary.errors += r.error_count;
                summary.unsupported += r.unsupported_format_count;
                summary.prunable += r.docs_prunable;
                summary.deleted += r.docs_deleted;
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

pub(crate) async fn run_index_async(
    ctx: &CliContext,
    source_id: Option<&str>,
    strict: bool,
    delete: bool,
) {
    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = config_loader.paths.data_dir.clone();

    // Per specs/05-surfaces.md §2: when daemon is running, submit a job per
    // resolved store instead of indexing embedded. Probed *before* store
    // resolution — like `source add`'s daemon branch
    // (`cli/src/cmds/source.rs`) — because the two paths resolve scope
    // differently: the daemon owns its own store set (see
    // `resolve_daemon_store_scope`), which may differ from whatever this
    // process's local database happens to contain (`LOCALDB_DAEMON_URL` can
    // point at a daemon with an entirely different data directory). Getting
    // this order wrong was Codex review round 2, finding 1: an explicit
    // daemon-valid `--store` used to be rejected against the local DB before
    // the daemon was ever asked, and an omitted `-s` submitted jobs for the
    // *local* store set instead of the daemon's. `/v1/jobs` is a
    // single-store API (server/src/handlers/jobs.rs), so a multi-store scope
    // becomes one POST per store here rather than a batched request.
    if let DaemonState::Running { base_url } = probe_daemon(&data_dir, ctx.daemon_url.as_deref()) {
        let store_names =
            resolve_daemon_store_scope(&base_url, ctx, StoreScopePolicy::AllStores).await;
        run_daemon_index(ctx, &base_url, &store_names, source_id).await;
        return;
    }

    // specs/05-surfaces.md §2.2: `-s` is repeatable and every store scoped by
    // it (or, absent `-s`, every store in the database) is indexed. Resolved
    // here, after the daemon probe above, so the embedded path still opens
    // the DB exactly once (via `load_app_db` at the top of this function)
    // and never pays for a local store lookup that the daemon branch would
    // have thrown away.
    let store_rows = resolve_store_scope(ctx, &db, StoreScopePolicy::AllStores).await;

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
    // an N-store run still constructs the embedder at most once, even across
    // a mid-store failure: `run_embedded_index_with` threads the embedder it
    // built through its own `WarnAndContinue` error paths too, so
    // `used_embedder` comes back `Some` as soon as the embedder exists,
    // regardless of whether that call went on to succeed or fail.
    //
    // This loop uses `IndexErrorMode::StrictExit`, not for embedder caching
    // (the caching now holds under either mode — see above), but for
    // `index`'s own semantics: `index` aborts the whole run the moment any
    // store fails (`exit_err` below), unlike `source add`'s auto-index loop
    // (`run_source_add_async`), which deliberately keeps going under
    // `WarnAndContinue` so one bad source doesn't fail the add.
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
            if delete {
                localdb_core::ingestion::DeletionPolicy::Prune
            } else {
                localdb_core::ingestion::DeletionPolicy::Retain
            },
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
/// `run_daemon_index`'s doc comment, finding 4) down to `source_id`'s owning
/// store (Codex review round 2, finding 2).
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

/// Submit one `/v1/jobs` request per resolved store to a running daemon and
/// report the submissions. `/v1/jobs` is single-store only
/// (`server/src/handlers/jobs.rs`'s `CreateJobRequest`), so there is no
/// batched request to make here — this is intentionally simple submit-and-
/// report, matching what the single-store path already did, looped.
///
/// When `source_id` is given, this always narrows to the owning store via
/// `resolve_daemon_source_owner` first — regardless of whether `store_names`
/// is already a single store. A single-store scope used to short-circuit
/// straight to submission with zero ownership verification, which meant
/// `index --store foo --source bogus-id` submitted a job (0 docs attached)
/// instead of exiting 3 like embedded mode's equivalent
/// (`run_embedded_index_with`, which always resolves the source's true owner
/// and checks it against scope) — precisely the daemon/embedded divergence
/// this whole command exists to eliminate (finding 4). The extra request
/// this costs in the common `-s one --source X` case is an accepted
/// trade-off for exact parity.
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
    store_names: &[String],
    source_id: Option<&str>,
) {
    let target_names: Vec<String> = match source_id {
        Some(sid) => match resolve_daemon_source_owner(base_url, store_names, sid).await {
            Ok(owner) => vec![owner],
            Err(e) => exit_err(&e, ctx.json),
        },
        None => store_names.to_vec(),
    };

    let mut submissions: Vec<(String, serde_json::Value)> = Vec::with_capacity(target_names.len());
    for store_name in &target_names {
        let url = format!("{}/v1/jobs", base_url);
        let mut body = json!({ "store_name": store_name });
        if let Some(sid) = source_id {
            body["source_id"] = serde_json::Value::String(sid.to_string());
        }
        match daemon_request_async(reqwest::Method::POST, &url, Some(body)).await {
            Ok(v) => submissions.push((store_name.clone(), v)),
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
