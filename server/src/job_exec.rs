//! Real ingestion engine for daemon-submitted jobs (issue #187 §1).
//!
//! Before this module existed, `POST /v1/jobs` and the URL refresh scheduler
//! both submitted a stub closure that returned `IndexJobStats::default()`
//! without ever running ingestion — a job would go `Pending -> Running ->
//! Completed` reporting all-zero stats while nothing was actually indexed.
//! `run_job` is a faithful port of the CLI's embedded composition loop
//! (`cli/src/cmds/index.rs`'s `run_embedded_index_with`) so the daemon does
//! the same real work the CLI does when running embedded, just scoped to a
//! single job invocation rather than a whole CLI process's store loop.
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
    } = deps;

    let all_sources = backend.list_sources(&store_row.id).await?;

    let sources_to_index: Vec<SourceRow> = match &scope {
        IndexJobScope::Store => all_sources,
        IndexJobScope::Source { source_id } => {
            match all_sources.into_iter().find(|s| &s.id == source_id) {
                Some(s) => vec![s],
                None => {
                    return Err(Error::SourceNotFound {
                        id: source_id.clone(),
                    })
                }
            }
        }
        IndexJobScope::Document { .. } => {
            // Not reachable today: `CreateJobRequest` has no `resource_id`
            // field, so no HTTP caller can construct this scope. Kept as an
            // explicit, honest error rather than a silent no-op in case a
            // future caller reaches it before document-scoped jobs are
            // implemented.
            return Err(Error::InvalidRequest {
                message: "document-scoped index jobs are not yet supported".to_string(),
            });
        }
    };

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

    let mut stats = IndexJobStats::default();

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
                stats.docs_deleted += r.docs_deleted;
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
            }
        }
    }

    Ok((stats, Some(embedder)))
}
