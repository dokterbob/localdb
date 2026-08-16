//! `run_job`: an embedder construction failure propagates as `Err` rather
//! than being swallowed — unlike per-source ingestion failures, a job can't
//! proceed at all without an embedder.

use localdb_core::config::schema::EmbeddingPolicy;
use localdb_core::ingestion::DeletionPolicy;
use localdb_core::{Error, IndexJobScope};
use serde_json::json;

use super::common::{fake_yaml, test_state};
use crate::job_exec::{run_job, JobExecDeps};

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
