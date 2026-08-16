//! Shared test fixtures for tenant tests.

use localdb_core::types::{SourceKind, StoreVisibility};
use localdb_core::{SourceRow, StoreBackend, StoreBackendConfig, StoreRow, VectorEncoding};

use crate::SqliteBackend;

/// Open a fresh backend at `path` and seed it with a single store
/// (`store-1`) and a single path source (`src-1`) — the minimal fixture
/// tenant tests build on before exercising a `TenantStore` handle.
pub(in crate::tenant) async fn backend_with_store_and_source(
    path: &std::path::Path,
) -> SqliteBackend {
    let backend = SqliteBackend::open(StoreBackendConfig::local_path(
        path.to_path_buf(),
        4,
        VectorEncoding::Float32,
    ))
    .await
    .unwrap();

    backend
        .upsert_store(&StoreRow {
            id: "store-1".to_string(),
            name: "notes".to_string(),
            visibility: StoreVisibility::Private,
            backend: "libsql".to_string(),
            indexing_policy: "{}".to_string(),
            policy_version: "v1".to_string(),
            acl: "{}".to_string(),
            created_at: "2026-07-01T00:00:00Z".to_string(),
        })
        .await
        .unwrap();
    backend
        .upsert_source(&SourceRow {
            id: "src-1".to_string(),
            store_id: "store-1".to_string(),
            kind: SourceKind::Path,
            root: Some("/docs".to_string()),
            url: None,
            include: vec![],
            exclude: vec![],
            preset: "prose".to_string(),
            refresh: None,
            created_at: "2026-07-01T00:00:00Z".to_string(),
            config_json: None,
        })
        .await
        .unwrap();

    backend
}
