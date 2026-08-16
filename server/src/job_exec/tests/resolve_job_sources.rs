//! `resolve_job_sources`: unknown source id and document scope rejection.

use localdb_core::{Error, IndexJobScope};

use super::common::test_state;
use crate::job_exec::resolve_job_sources;

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
