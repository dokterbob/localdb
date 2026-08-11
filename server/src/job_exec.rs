//! Real ingestion engine for daemon-submitted jobs (issue #187 §1).
//!
//! Before this module existed, `POST /v1/jobs` and the URL refresh scheduler
//! both submitted a stub closure that returned `IndexJobStats::default()`
//! without ever running ingestion — a job would go `Pending -> Running ->
//! Completed` reporting all-zero stats while nothing was actually indexed.
//! `run_job` is a faithful port of what was originally the CLI's own
//! embedded composition loop (`run_embedded_index_with`) so the daemon does
//! the same real work the CLI does when running embedded, just scoped to a
//! single job invocation rather than a whole CLI process's store loop. As of
//! issue #187 stage 3, the CLI's embedded `index`/`source add` paths run
//! through this exact function too (via a local `JobQueue`,
//! `cli/src/job_attach.rs`) rather than a separate copy — `run_job` is now
//! the one engine both surfaces share.
//!
//! Error handling follows specs §5's WARN-per-file rule: a per-source
//! failure increments `IndexJobStats::error_count` and is logged via
//! `tracing::warn!`, never aborting the run. Only a failure that prevents
//! the whole job from proceeding at all (cannot list sources, cannot build
//! the embedder, cannot open the store handle, an unresolvable job scope)
//! returns `Err` — the caller fails the job honestly instead of reporting
//! fabricated success.

use std::path::Path;
use std::sync::Arc;

use fetch::HttpUrlFetcher;
use localdb_core::{
    chunker::ChunkerConfig,
    config::{policy::compute_policy_version, schema::RawConfig},
    ingestion::{
        run_source_ingestion, DeletionPolicy, DocumentIndex, IngestionConfig, SourceIngestionDeps,
    },
    ingestor::Ingestor,
    source_row_to_source, Embedder, Error, IndexJobScope, IndexJobStats, ProgressSink, SourceRow,
    StoreBackend, StoreRow,
};

/// Dependencies [`run_job`] needs, borrowed for the duration of a single job.
pub struct JobExecDeps<'a> {
    pub backend: &'a dyn StoreBackend,
    pub yaml: &'a RawConfig,
    pub models_dir: &'a Path,
    /// An already-built embedder to reuse (e.g. across a caller's own
    /// multi-store loop, mirroring the CLI's `run_embedded_index_with`
    /// threading). `None` builds one via `embed::create_embedder`.
    pub embedder: Option<Arc<dyn Embedder>>,
    /// Progress sink threaded into `run_source_ingestion`. `None` in this
    /// stage — a later stage wires a real sink that reports into the
    /// `IndexJob`'s pollable state.
    pub progress: Option<ProgressSink>,
    /// Optional per-source-error hook (issue #187 stage 3), invoked in
    /// addition to the standard `tracing::warn!` log line for the two
    /// per-source failure sites below. Exists so the CLI's embedded index
    /// path — which predates this shared engine and has stable, integration-
    /// test-pinned `eprintln!` wording that differs between `index`
    /// (StrictExit) and `source add`'s auto-index (WarnAndContinue) — can
    /// reproduce its exact historical diagnostics while still running
    /// through this one engine. `None` for HTTP-submitted daemon jobs, which
    /// only ever log via `tracing`.
    pub on_source_error: Option<OnSourceError>,
}

/// Type of [`JobExecDeps::on_source_error`], factored out so the field
/// itself doesn't trip clippy's `type_complexity` lint.
pub type OnSourceError = Arc<dyn Fn(&str, SourceError<'_>) + Send + Sync>;

/// Which per-source failure occurred, passed to
/// [`JobExecDeps::on_source_error`] so a caller can render mode-specific
/// diagnostic text without this engine needing to know about CLI
/// presentation concerns.
pub enum SourceError<'a> {
    /// The source's chunker preset failed to resolve; ingestion for it never
    /// started.
    InvalidChunkerPreset { preset: &'a str, error: &'a Error },
    /// `run_source_ingestion` itself returned an error for this source as a
    /// whole.
    Ingestion { error: &'a Error },
}

/// Resolve which sources `scope` selects for `store_id`, without running any
/// indexing.
///
/// Shared by [`run_job`] (which calls this internally to build its working
/// set) and CLI callers that need to know up front whether a job would have
/// anything to do at all — e.g. to decide whether to pay for building a
/// ~706 MB local embedding model just to report "no sources to index".
pub async fn resolve_job_sources(
    backend: &dyn StoreBackend,
    store_id: &str,
    scope: &IndexJobScope,
) -> Result<Vec<SourceRow>, Error> {
    let all_sources = backend.list_sources(store_id).await?;

    match scope {
        IndexJobScope::Store => Ok(all_sources),
        IndexJobScope::Source { source_id } => {
            match all_sources.into_iter().find(|s| &s.id == source_id) {
                Some(s) => Ok(vec![s]),
                None => Err(Error::SourceNotFound {
                    id: source_id.clone(),
                }),
            }
        }
        IndexJobScope::Document { .. } => Err(Error::InvalidRequest {
            message: "document-scoped index jobs are not yet supported".to_string(),
        }),
    }
}

/// Run one indexing job for `store_row`, scoped by `scope`, against real
/// ingestion machinery.
///
/// Returns the accumulated stats alongside the embedder actually used
/// (`Some` as soon as one was built or reused, `None` only when the scope
/// resolved to zero sources), so a caller running multiple jobs in sequence
/// can reuse it rather than paying for repeated construction (for the
/// default `local` provider that's a ~706 MB one-time model load).
pub async fn run_job(
    store_row: &StoreRow,
    scope: IndexJobScope,
    deletion: DeletionPolicy,
    deps: JobExecDeps<'_>,
) -> Result<(IndexJobStats, Option<Arc<dyn Embedder>>), Error> {
    let JobExecDeps {
        backend,
        yaml,
        models_dir,
        embedder,
        progress,
        on_source_error,
    } = deps;

    // `resolve_job_sources` folds in the `IndexJobScope::Document` rejection
    // too — not reachable today (`CreateJobRequest` has no `resource_id`
    // field, so no HTTP caller can construct this scope), kept as an
    // explicit, honest error rather than a silent no-op in case a future
    // caller reaches it before document-scoped jobs are implemented.
    let sources_to_index = resolve_job_sources(backend, &store_row.id, &scope).await?;

    if sources_to_index.is_empty() {
        return Ok((IndexJobStats::default(), embedder));
    }

    let policy = yaml.defaults.indexing.clone();
    let current_policy_version = compute_policy_version(&yaml.defaults.indexing);
    if store_row.policy_version != current_policy_version {
        let new_indexing_policy =
            serde_json::to_string(&policy).unwrap_or_else(|_| store_row.indexing_policy.clone());
        let updated_store = StoreRow {
            policy_version: current_policy_version.clone(),
            indexing_policy: new_indexing_policy,
            ..store_row.clone()
        };
        if let Err(e) = backend.upsert_store(&updated_store).await {
            tracing::warn!(
                "job_exec: failed to update policy_version for store '{}': {}",
                store_row.name,
                e
            );
        }
    }

    let ingestion_cfg = IngestionConfig {
        store_id: store_row.id.clone(),
        policy_version: current_policy_version,
        chunker: ChunkerConfig::prose(),
    };

    let embedder: Arc<dyn Embedder> = match embedder {
        Some(e) => e,
        None => {
            let built = embed::create_embedder(
                &yaml.defaults.indexing.embedding,
                &yaml.providers,
                Some(models_dir),
            )?;
            Arc::from(built)
        }
    };

    // Validate the parser chain once up front, fail-fast — mirrors the
    // CLI's embedded path. Parser instances aren't `Clone`, so each source
    // below rebuilds its own owned chain from the same `policy.parsers` ids
    // rather than sharing this one.
    extract::build_chain(&policy.parsers)?;

    let handle = backend.retrieval_store(&store_row.id).await?;
    let existing = handle.list_indexed_documents().await?;
    let mut doc_index = DocumentIndex::from_records(existing);

    let url_fetcher = HttpUrlFetcher::new()?;
    // Destination-restricted client for locators that come from *content*
    // rather than from the operator (a feed entry's `<link>`) — see
    // `ingest::FeedIngestor::new`'s doc comment.
    let entry_fetcher = HttpUrlFetcher::new_public_only()?;

    let mut stats = IndexJobStats {
        sources_count: sources_to_index.len() as u64,
        ..Default::default()
    };

    for rt_source in &sources_to_index {
        let source = source_row_to_source(rt_source);
        let chunker = match ChunkerConfig::from_preset(&source.source_preset) {
            Ok(c) => c,
            Err(e) => {
                stats.error_count += 1;
                tracing::warn!(
                    "job_exec: invalid chunker preset '{}' for source {}: {}",
                    source.source_preset,
                    rt_source.id,
                    e
                );
                if let Some(hook) = &on_source_error {
                    hook(
                        &rt_source.id,
                        SourceError::InvalidChunkerPreset {
                            preset: &source.source_preset,
                            error: &e,
                        },
                    );
                }
                continue;
            }
        };
        let cfg = IngestionConfig {
            chunker,
            ..ingestion_cfg.clone()
        };

        let parser_chain =
            extract::build_chain(&policy.parsers).expect("parser chain already validated above");
        let ingestor: Box<dyn Ingestor> = ingest::build_ingestor_for_spec(
            &source.spec,
            parser_chain,
            &url_fetcher,
            &entry_fetcher,
        );

        let source_deps = SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: handle.as_ref(),
            embedder: embedder.as_ref(),
            config: &cfg,
            progress: progress.clone(),
            deletion,
        };

        match run_source_ingestion(&source, ingestor.as_ref(), source_deps).await {
            Ok(r) => {
                stats.docs_seen += r.docs_seen;
                stats.docs_indexed += r.docs_indexed;
                stats.docs_skipped += r.docs_skipped;
                stats.docs_deleted += r.docs_deleted;
                stats.docs_prunable += r.docs_prunable;
                stats.chunks_written += r.chunks_written;
                stats.unsupported_format_count += r.unsupported_format_count;
                stats.error_count += r.error_count;
            }
            Err(e) => {
                stats.error_count += 1;
                tracing::warn!(
                    "job_exec: ingestion error for source {}: {}",
                    rt_source.id,
                    e
                );
                if let Some(hook) = &on_source_error {
                    hook(&rt_source.id, SourceError::Ingestion { error: &e });
                }
            }
        }
    }

    Ok((stats, Some(embedder)))
}
