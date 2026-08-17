//! `run_job`: empty scope short-circuit — a store with zero sources returns
//! default stats and hands the already-built embedder straight back.

use std::sync::Arc;

use localdb_core::ingestion::DeletionPolicy;
use localdb_core::{Embedder, IndexJobScope, IndexJobStats};

use super::common::{fake_yaml, test_state};
use crate::job_exec::{run_job, JobExecDeps};

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
            fetchers: None,
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
