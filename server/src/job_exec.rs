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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use localdb_core::config::schema::{
        DefaultsConfig, EmbeddingPolicy, IndexingPolicyConfig, RawConfig,
    };
    use localdb_core::{DocumentInfo, RetrievalStore, TableSize};
    use serde_json::json;
    use tempfile::TempDir;

    use crate::state::AppState;

    fn fake_yaml() -> RawConfig {
        RawConfig {
            version: 1,
            schema: None,
            server: Default::default(),
            paths: Default::default(),
            defaults: DefaultsConfig {
                indexing: IndexingPolicyConfig {
                    chunking: Default::default(),
                    embedding: EmbeddingPolicy {
                        provider: "fake".to_string(),
                        model: "default".to_string(),
                    },
                    ..Default::default()
                },
            },
            providers: vec![],
        }
    }

    /// Real backend + state, wired exactly like `AppState::new` (fake
    /// embedder, no network/model download) — mirrors
    /// `server/src/handlers/tests/common.rs::make_state_with_fake_config`,
    /// duplicated here rather than shared because that helper is private to
    /// the `handlers::tests` module tree.
    async fn test_state() -> (TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let queue = crate::job_queue::JobQueue::new();
        let state = AppState::new(
            fake_yaml(),
            dir.path().to_path_buf(),
            dir.path().join("models"),
            queue.clone(),
            crate::scheduler::UrlRefreshScheduler::new(queue),
        )
        .await
        .unwrap();
        (dir, state)
    }

    // --- resolve_job_sources -------------------------------------------

    #[tokio::test]
    async fn resolve_job_sources_unknown_source_id_is_source_not_found() {
        let (_dir, state) = test_state().await;
        state.add_store("docs", "private").await.unwrap();
        let store = state
            .backend()
            .get_store_by_name("docs")
            .await
            .unwrap()
            .unwrap();

        let err = resolve_job_sources(
            state.backend(),
            &store.id,
            &IndexJobScope::Source {
                source_id: "01HRQHB7FN3WMX4AZDV3S9VCTZ".to_string(),
            },
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, Error::SourceNotFound { ref id } if id == "01HRQHB7FN3WMX4AZDV3S9VCTZ"),
            "expected SourceNotFound, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn resolve_job_sources_document_scope_is_rejected_as_not_yet_supported() {
        let (_dir, state) = test_state().await;
        state.add_store("docs", "private").await.unwrap();
        let store = state
            .backend()
            .get_store_by_name("docs")
            .await
            .unwrap()
            .unwrap();

        let err = resolve_job_sources(
            state.backend(),
            &store.id,
            &IndexJobScope::Document {
                resource_id: "doc-1".to_string(),
            },
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, Error::InvalidRequest { ref message } if message.contains("document-scoped")),
            "expected an explicit 'document-scoped index jobs are not yet supported' error, got: {err:?}"
        );
    }

    // --- run_job: empty scope short-circuit -----------------------------

    #[tokio::test]
    async fn run_job_with_no_sources_returns_default_stats_and_passes_the_embedder_through() {
        let (_dir, state) = test_state().await;
        state.add_store("docs", "private").await.unwrap();
        let store = state
            .backend()
            .get_store_by_name("docs")
            .await
            .unwrap()
            .unwrap();
        let yaml = fake_yaml();
        let embedder: Arc<dyn Embedder> = Arc::new(localdb_core::FakeEmbedder::new(128));

        let (stats, returned_embedder) = run_job(
            &store,
            IndexJobScope::Store,
            DeletionPolicy::Retain,
            JobExecDeps {
                backend: state.backend(),
                yaml: &yaml,
                models_dir: state.models_dir(),
                embedder: Some(embedder.clone()),
                progress: None,
                on_source_error: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(
            stats,
            IndexJobStats::default(),
            "a store with zero sources must report all-zero stats, never fabricated success"
        );
        assert!(
            Arc::ptr_eq(
                &returned_embedder.expect("embedder must be handed back unchanged"),
                &embedder
            ),
            "an unused, already-built embedder must be passed straight through, not rebuilt"
        );
    }

    // --- run_job: embedder construction failure propagates --------------

    /// When no embedder is threaded in, `run_job` builds one via
    /// `embed::create_embedder` and must propagate a construction failure
    /// as `Err` rather than swallowing it — unlike per-source ingestion
    /// failures, a job can't proceed at all without an embedder. Uses
    /// `perplexity` with no matching `providers:` entry for a deterministic,
    /// fully offline failure (no network call is ever attempted).
    #[tokio::test]
    async fn run_job_propagates_an_embedder_construction_failure() {
        let (dir, state) = test_state().await;
        state.add_store("docs", "private").await.unwrap();
        let root = dir.path().join("some-root");
        std::fs::create_dir(&root).unwrap();
        state
            .add_source(
                "docs",
                "path",
                json!({ "root": root.to_str().unwrap() }),
                "prose",
                None,
            )
            .await
            .unwrap();
        let store = state
            .backend()
            .get_store_by_name("docs")
            .await
            .unwrap()
            .unwrap();

        let mut yaml = fake_yaml();
        yaml.defaults.indexing.embedding = EmbeddingPolicy {
            provider: "perplexity".to_string(),
            model: "default".to_string(),
        };

        let result = run_job(
            &store,
            IndexJobScope::Store,
            DeletionPolicy::Retain,
            JobExecDeps {
                backend: state.backend(),
                yaml: &yaml,
                models_dir: state.models_dir(),
                embedder: None,
                progress: None,
                on_source_error: None,
            },
        )
        .await;

        match result {
            Err(err) => assert!(
                matches!(err, Error::InvalidConfig { ref message } if message.contains("perplexity")),
                "expected the missing-provider-block config error, got: {err:?}"
            ),
            Ok(_) => panic!("expected run_job to propagate the embedder construction failure"),
        }
    }

    // --- run_job: policy-version persistence failure is warn-and-continue --

    /// A `StoreBackend` wrapper that runs every call against a real inner
    /// backend except `upsert_store`, which always fails — the only way to
    /// exercise `run_job`'s "persist the refreshed policy_version" failure
    /// branch (job_exec.rs's `tracing::warn!` on `backend.upsert_store`
    /// error) without a flaky, platform-dependent trick like corrupting the
    /// SQLite file on disk.
    struct FailingUpsertBackend {
        inner: Arc<dyn StoreBackend>,
    }

    #[async_trait]
    impl StoreBackend for FailingUpsertBackend {
        async fn open(_config: localdb_core::StoreBackendConfig) -> Result<Self, Error> {
            unimplemented!("never constructed via the trait's own open()")
        }

        async fn upsert_store(&self, _store: &StoreRow) -> Result<(), Error> {
            Err(Error::Internal {
                message: "simulated upsert_store failure".to_string(),
                correlation_id: "test_failing_upsert_backend".to_string(),
            })
        }
        async fn delete_store(&self, id: &str) -> Result<bool, Error> {
            self.inner.delete_store(id).await
        }
        async fn get_store(&self, id: &str) -> Result<Option<StoreRow>, Error> {
            self.inner.get_store(id).await
        }
        async fn get_store_by_name(&self, name: &str) -> Result<Option<StoreRow>, Error> {
            self.inner.get_store_by_name(name).await
        }
        async fn list_stores(&self) -> Result<Vec<StoreRow>, Error> {
            self.inner.list_stores().await
        }
        async fn upsert_source(&self, source: &SourceRow) -> Result<(), Error> {
            self.inner.upsert_source(source).await
        }
        async fn delete_source(&self, id: &str) -> Result<bool, Error> {
            self.inner.delete_source(id).await
        }
        async fn get_source(&self, id: &str) -> Result<Option<SourceRow>, Error> {
            self.inner.get_source(id).await
        }
        async fn list_sources(&self, store_id: &str) -> Result<Vec<SourceRow>, Error> {
            self.inner.list_sources(store_id).await
        }
        async fn find_source_by_root_or_url(
            &self,
            value: &str,
            store_id: &str,
        ) -> Result<Option<SourceRow>, Error> {
            self.inner.find_source_by_root_or_url(value, store_id).await
        }
        async fn find_document(&self, doc_id: &str) -> Result<Option<DocumentInfo>, Error> {
            self.inner.find_document(doc_id).await
        }
        async fn retrieval_store(&self, store_id: &str) -> Result<Arc<dyn RetrievalStore>, Error> {
            self.inner.retrieval_store(store_id).await
        }
        async fn largest_tables(&self, limit: usize) -> Result<Vec<TableSize>, Error> {
            self.inner.largest_tables(limit).await
        }
    }

    #[tokio::test]
    async fn run_job_continues_when_persisting_the_refreshed_policy_version_fails() {
        let (dir, state) = test_state().await;
        state.add_store("docs", "private").await.unwrap();
        // An existing, empty directory: a valid path source that indexes
        // zero documents without touching the network — this test's point
        // is the policy-version-persistence failure, not ingestion itself.
        let root = dir.path().join("empty-root");
        std::fs::create_dir(&root).unwrap();
        state
            .add_source(
                "docs",
                "path",
                json!({ "root": root.to_str().unwrap() }),
                "prose",
                None,
            )
            .await
            .unwrap();

        let mut store = state
            .backend()
            .get_store_by_name("docs")
            .await
            .unwrap()
            .unwrap();
        // Force a policy-version mismatch so `run_job` attempts the
        // refresh-and-persist path at all.
        store.policy_version = "stale-version".to_string();

        let yaml = fake_yaml();
        let wrapper: Arc<dyn StoreBackend> = Arc::new(FailingUpsertBackend {
            inner: state.backend_arc(),
        });

        let (stats, _embedder) = run_job(
            &store,
            IndexJobScope::Store,
            DeletionPolicy::Retain,
            JobExecDeps {
                backend: wrapper.as_ref(),
                yaml: &yaml,
                models_dir: state.models_dir(),
                embedder: None,
                progress: None,
                on_source_error: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(
            stats.sources_count, 1,
            "the job must still process the store's source despite the policy_version \
             persistence failure — that failure is logged and swallowed, never fatal"
        );
    }
}
