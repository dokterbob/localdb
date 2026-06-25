use std::sync::Arc;

use localdb_core::types::{SourceKind, StoreVisibility};
use localdb_core::VectorEncoding;
use tempfile::tempdir;

use crate::db::LibsqlDb;

use super::{RuntimeStateApi, SourceRow, StoreRow};

async fn make_api() -> (tempfile::TempDir, RuntimeStateApi) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
        .await
        .unwrap();
    let api = RuntimeStateApi::new(Arc::new(db));
    (dir, api)
}

fn make_store(id: &str, name: &str) -> StoreRow {
    StoreRow {
        id: id.to_string(),
        name: name.to_string(),
        visibility: StoreVisibility::Private,
        backend: "libsql".to_string(),
        indexing_policy: "{}".to_string(),
        policy_version: "v1".to_string(),
        acl: "{}".to_string(),
        created_at: "2026-06-25T12:00:00Z".to_string(),
    }
}

fn make_path_source(id: &str, store_id: &str, root: &str) -> SourceRow {
    SourceRow {
        id: id.to_string(),
        store_id: store_id.to_string(),
        kind: SourceKind::Path,
        root: Some(root.to_string()),
        url: None,
        include: vec![],
        exclude: vec![],
        preset: "prose".to_string(),
        refresh: None,
        created_at: "2026-06-25T12:00:00Z".to_string(),
    }
}

fn make_url_source(id: &str, store_id: &str, url: &str) -> SourceRow {
    SourceRow {
        id: id.to_string(),
        store_id: store_id.to_string(),
        kind: SourceKind::Url,
        root: None,
        url: Some(url.to_string()),
        include: vec![],
        exclude: vec![],
        preset: "prose".to_string(),
        refresh: Some("24h".to_string()),
        created_at: "2026-06-25T12:00:00Z".to_string(),
    }
}

#[tokio::test]
async fn list_stores_empty_on_fresh_db() {
    let (_dir, api) = make_api().await;
    assert!(api.list_stores().await.unwrap().is_empty());
}

#[tokio::test]
async fn upsert_and_get_store_round_trips() {
    let (_dir, api) = make_api().await;
    let s = make_store("store-1", "notes");
    api.upsert_store(&s).await.unwrap();
    let got = api.get_store("store-1").await.unwrap().unwrap();
    assert_eq!(got, s);
}

#[tokio::test]
async fn get_nonexistent_store_returns_none() {
    let (_dir, api) = make_api().await;
    assert!(api.get_store("nope").await.unwrap().is_none());
}

#[tokio::test]
async fn get_store_by_name_finds_it() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-1", "notes"))
        .await
        .unwrap();
    let got = api.get_store_by_name("notes").await.unwrap().unwrap();
    assert_eq!(got.id, "store-1");
}

#[tokio::test]
async fn upsert_store_overwrites_existing() {
    let (_dir, api) = make_api().await;
    let mut s = make_store("store-1", "notes");
    api.upsert_store(&s).await.unwrap();
    s.visibility = StoreVisibility::Shared;
    api.upsert_store(&s).await.unwrap();
    let got = api.get_store("store-1").await.unwrap().unwrap();
    assert_eq!(got.visibility, StoreVisibility::Shared);
}

#[tokio::test]
async fn delete_existing_store_returns_true() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-1", "notes"))
        .await
        .unwrap();
    assert!(api.delete_store("store-1").await.unwrap());
    assert!(api.get_store("store-1").await.unwrap().is_none());
}

#[tokio::test]
async fn delete_nonexistent_store_returns_false() {
    let (_dir, api) = make_api().await;
    assert!(!api.delete_store("nope").await.unwrap());
}

#[tokio::test]
async fn list_stores_returns_all_alphabetical_by_name() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("id-c", "charlie"))
        .await
        .unwrap();
    api.upsert_store(&make_store("id-a", "alpha"))
        .await
        .unwrap();
    api.upsert_store(&make_store("id-b", "bravo"))
        .await
        .unwrap();
    let stores = api.list_stores().await.unwrap();
    assert_eq!(stores.len(), 3);
    assert_eq!(stores[0].name, "alpha");
    assert_eq!(stores[1].name, "bravo");
    assert_eq!(stores[2].name, "charlie");
}

#[tokio::test]
async fn unique_store_name_enforced() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("id-1", "notes"))
        .await
        .unwrap();
    let result = api.upsert_store(&make_store("id-2", "notes")).await;
    assert!(result.is_err(), "duplicate name should fail");
}

#[tokio::test]
async fn upsert_source_requires_existing_store() {
    let (_dir, api) = make_api().await;
    let result = api
        .upsert_source(&make_path_source("src-1", "missing-store", "/docs"))
        .await;
    assert!(result.is_err(), "FK should reject orphan source");
}

#[tokio::test]
async fn upsert_and_get_path_source_round_trips() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-1", "notes"))
        .await
        .unwrap();
    let s = make_path_source("src-1", "store-1", "/docs");
    api.upsert_source(&s).await.unwrap();
    let got = api.get_source("src-1").await.unwrap().unwrap();
    assert_eq!(got, s);
}

#[tokio::test]
async fn upsert_and_get_url_source_round_trips() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-1", "notes"))
        .await
        .unwrap();
    let s = make_url_source("src-1", "store-1", "https://example.com");
    api.upsert_source(&s).await.unwrap();
    let got = api.get_source("src-1").await.unwrap().unwrap();
    assert_eq!(got, s);
}

#[tokio::test]
async fn check_constraint_rejects_path_kind_without_root() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-1", "notes"))
        .await
        .unwrap();
    let mut bad = make_path_source("src-1", "store-1", "/docs");
    bad.root = None;
    let result = api.upsert_source(&bad).await;
    assert!(result.is_err(), "CHECK should reject path without root");
}

#[tokio::test]
async fn check_constraint_rejects_url_kind_with_root() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-1", "notes"))
        .await
        .unwrap();
    let mut bad = make_url_source("src-1", "store-1", "https://example.com");
    bad.root = Some("/docs".to_string());
    let result = api.upsert_source(&bad).await;
    assert!(
        result.is_err(),
        "CHECK should reject url kind with root set"
    );
}

#[tokio::test]
async fn list_sources_filters_by_store_id() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-1", "store-1"))
        .await
        .unwrap();
    api.upsert_store(&make_store("store-2", "store-2"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-1", "store-1", "/a"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-2", "store-1", "/b"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-3", "store-2", "/c"))
        .await
        .unwrap();
    let s1 = api.list_sources("store-1").await.unwrap();
    assert_eq!(s1.len(), 2);
    let s2 = api.list_sources("store-2").await.unwrap();
    assert_eq!(s2.len(), 1);
}

#[tokio::test]
async fn delete_source_returns_true_then_false() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-1", "notes"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-1", "store-1", "/a"))
        .await
        .unwrap();
    assert!(api.delete_source("src-1").await.unwrap());
    assert!(!api.delete_source("src-1").await.unwrap());
}

#[tokio::test]
async fn delete_sources_for_store_returns_removed_count() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-1", "notes"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-1", "store-1", "/a"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-2", "store-1", "/b"))
        .await
        .unwrap();
    let n = api.delete_sources_for_store("store-1").await.unwrap();
    assert_eq!(n, 2);
}

#[tokio::test]
async fn delete_store_cascades_to_sources() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-1", "notes"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-1", "store-1", "/a"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-2", "store-1", "/b"))
        .await
        .unwrap();
    api.delete_store("store-1").await.unwrap();
    let remaining = api.list_sources("store-1").await.unwrap();
    assert!(
        remaining.is_empty(),
        "FK CASCADE should remove sources with parent store"
    );
}

#[tokio::test]
async fn find_source_by_root_finds_it() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-1", "notes"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-1", "store-1", "/docs/notes"))
        .await
        .unwrap();
    let found = api
        .find_source_by_root_or_url("/docs/notes", None)
        .await
        .unwrap();
    assert_eq!(found.unwrap().id, "src-1");
}

#[tokio::test]
async fn find_source_by_url_finds_it() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-1", "notes"))
        .await
        .unwrap();
    api.upsert_source(&make_url_source("src-1", "store-1", "https://example.com"))
        .await
        .unwrap();
    let found = api
        .find_source_by_root_or_url("https://example.com", None)
        .await
        .unwrap();
    assert_eq!(found.unwrap().id, "src-1");
}

#[tokio::test]
async fn find_source_scoped_to_store() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("s1", "s1")).await.unwrap();
    api.upsert_store(&make_store("s2", "s2")).await.unwrap();
    api.upsert_source(&make_path_source("src-a", "s1", "/shared"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-b", "s2", "/shared"))
        .await
        .unwrap();
    let f1 = api
        .find_source_by_root_or_url("/shared", Some("s1"))
        .await
        .unwrap();
    assert_eq!(f1.unwrap().id, "src-a");
    let f2 = api
        .find_source_by_root_or_url("/shared", Some("s2"))
        .await
        .unwrap();
    assert_eq!(f2.unwrap().id, "src-b");
}

#[tokio::test]
async fn unique_root_per_store_enforced() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("store-1", "notes"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-1", "store-1", "/docs"))
        .await
        .unwrap();
    let result = api
        .upsert_source(&make_path_source("src-2", "store-1", "/docs"))
        .await;
    assert!(
        result.is_err(),
        "partial UNIQUE (store_id, root) should reject duplicate"
    );
}

#[tokio::test]
async fn same_root_across_different_stores_allowed() {
    let (_dir, api) = make_api().await;
    api.upsert_store(&make_store("s1", "s1")).await.unwrap();
    api.upsert_store(&make_store("s2", "s2")).await.unwrap();
    api.upsert_source(&make_path_source("src-a", "s1", "/docs"))
        .await
        .unwrap();
    api.upsert_source(&make_path_source("src-b", "s2", "/docs"))
        .await
        .unwrap();
}
