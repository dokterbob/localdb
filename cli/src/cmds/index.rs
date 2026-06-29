use fetch::HttpUrlFetcher;
use localdb_core::{config::policy::compute_policy_version, Error, SourceRow, StoreRow};
use serde_json::json;

use crate::{
    app_db::{load_app_db, resolve_store_name},
    daemon_client::{daemon_request_async, probe_daemon, CliContext, DaemonState},
    normalize::{exit_err, print_json, source_row_to_core_source, validate_store_name},
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

impl IndexErrorMode {
    fn warn(self) -> bool {
        self == Self::WarnAndContinue
    }
}

pub(crate) async fn run_embedded_index(
    ctx: &CliContext,
    store_row: &StoreRow,
    source_id: Option<&str>,
    mode: IndexErrorMode,
) -> Result<IndexSummary, Error> {
    use localdb_core::{
        chunker::ChunkerConfig,
        ingestion::{run_ingestion_for_source, DocumentIndex, IngestionConfig},
    };

    macro_rules! warn_or_default {
        ($expr:expr, $fmt:literal) => {
            match $expr {
                Ok(value) => value,
                Err(e) => {
                    let error = Error::from(e);
                    if mode.warn() {
                        eprintln!($fmt, error);
                        return Ok(IndexSummary::default());
                    }
                    return Err(error);
                }
            }
        };
    }

    let (config_loader, db) = load_app_db(ctx).await;
    let all_sources = warn_or_default!(
        db.backend().list_sources(&store_row.id).await,
        "warning: cannot list sources for auto-index: {}"
    );

    let sources_to_index: Vec<SourceRow> = if let Some(sid) = source_id {
        match all_sources.into_iter().find(|s| s.id == sid) {
            Some(s) => vec![s],
            None if mode.warn() => return Ok(IndexSummary::default()),
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
        return Ok(IndexSummary::default());
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

    let embedder = warn_or_default!(
        embed::create_embedder(
            &config_loader.config.defaults.indexing.embedding,
            &config_loader.config.providers,
            Some(&config_loader.paths.models_dir),
        ),
        "warning: cannot create embedder for auto-index: {}"
    );
    let extractor = warn_or_default!(
        extract::ChainExtractor::from_ids(&policy.parsers),
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
        let chunker = match ChunkerConfig::from_preset(&source.source_kind_preset) {
            Ok(chunker) => chunker,
            Err(e) => {
                summary.errors += 1;
                if mode.warn() {
                    eprintln!(
                        "warning: invalid chunker preset '{}' for source {}: {}",
                        source.source_kind_preset, rt_source.id, e
                    );
                } else {
                    eprintln!(
                        "error indexing source {}: invalid chunker preset '{}': {}",
                        rt_source.id, source.source_kind_preset, e
                    );
                }
                continue;
            }
        };
        let cfg = IngestionConfig {
            chunker,
            ..ingestion_cfg.clone()
        };
        let sink = crate::progress::build_progress_sink(ctx.json);
        match run_ingestion_for_source(
            &source,
            &mut doc_index,
            handle.as_ref(),
            embedder.as_ref(),
            &cfg,
            &extractor,
            Some(&url_fetcher),
            sink,
        )
        .await
        {
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
    // A9-safety: validate --store name if given.
    if let Some(store_name) = ctx.stores.first() {
        if let Err(e) = validate_store_name(store_name) {
            exit_err(&e, ctx.json);
        }
    }

    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = config_loader.paths.data_dir.clone();
    let store_name = resolve_store_name(ctx, &db).await;

    // Per specs/05-surfaces.md §2: when daemon is running, submit a job and poll.
    if let DaemonState::Running { base_url } = probe_daemon(&data_dir, ctx.daemon_url.as_deref()) {
        let url = format!("{}/v1/jobs", base_url);
        let mut body = json!({ "store_name": store_name });
        if let Some(sid) = source_id {
            body["source_id"] = serde_json::Value::String(sid.to_string());
        }
        match daemon_request_async(reqwest::Method::POST, &url, Some(body)).await {
            Ok(v) => {
                if ctx.json {
                    print_json(&v);
                } else {
                    let job_id = v.get("id").and_then(|i| i.as_str()).unwrap_or("?");
                    println!(
                        "Index job submitted to daemon: {} (poll with status)",
                        job_id
                    );
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    let store_row = match db.backend().get_store_by_name(&store_name).await {
        Ok(Some(s)) => s,
        Ok(None) => exit_err(
            &Error::StoreNotFound {
                id: store_name.clone(),
            },
            ctx.json,
        ),
        Err(e) => exit_err(&e, ctx.json),
    };

    let summary =
        match run_embedded_index(ctx, &store_row, source_id, IndexErrorMode::StrictExit).await {
            Ok(summary) => summary,
            Err(e) => exit_err(&e, ctx.json),
        };

    if !summary.has_sources {
        if ctx.json {
            print_json(&json!({ "status": "ok", "message": "no sources to index" }));
        } else {
            println!("No sources to index on store '{}'.", store_name);
        }
        return;
    }

    let status = if strict && summary.errors > 0 {
        "error"
    } else {
        "ok"
    };
    if ctx.json {
        print_json(&json!({
            "status": status,
            "docs_indexed": summary.indexed,
            "docs_skipped": summary.skipped,
            "chunks_written": summary.chunks,
            "unsupported": summary.unsupported,
            "errors": summary.errors,
        }));
    } else {
        println!(
            "Index complete: {} indexed, {} skipped, {} chunks written, {} unsupported, {} errors",
            summary.indexed, summary.skipped, summary.chunks, summary.unsupported, summary.errors
        );
    }
    if strict && summary.errors > 0 {
        std::process::exit(2);
    }
}
