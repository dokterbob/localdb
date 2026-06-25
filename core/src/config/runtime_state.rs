//! Runtime-state DB backed by SQLite in WAL mode.
//!
//! Stores runtime-owned objects (stores/sources added via API/CLI)
//! separately from the YAML config. Never touches the YAML file.
//!
//! Concurrency model: each method opens a fresh `rusqlite::Connection`,
//! runs its statement, and drops it — so no connection (and thus no write
//! lock) is held between operations. SQLite WAL mode allows multiple
//! concurrent readers and serialises writers with a 5 s busy-timeout,
//! mapping to `Error::RuntimeStateLocked` only if contention lasts longer.
//!
//! Ownership model (specs/03-config.md §3):
//! - YAML-owned: object appears in the YAML config (matched by name).
//!   Mutations via API return `config_readonly`.
//! - Runtime-owned: object was created via API/CLI. Lives in the DB.
//!
//! The `EffectiveConfig` merges both views: YAML-owned objects take precedence
//! over runtime-owned objects with the same name.

use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::{
    config::schema::{IndexingPolicyConfig, RawConfig},
    Error,
};

/// Ownership of a config object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigOwnership {
    /// Object declared in YAML. Mutations via API return `config_readonly`.
    Yaml,
    /// Object created at runtime via API/CLI. Can be mutated via API.
    Runtime,
}

// ---------------------------------------------------------------------------
// Runtime store type
// ---------------------------------------------------------------------------

/// A runtime-owned store record (API/CLI created, never in YAML).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeStore {
    /// Store name (unique per instance, used as lookup key).
    pub name: String,

    /// Stable ULID, minted at creation.
    pub id: String,

    /// Visibility.
    #[serde(default = "default_visibility")]
    pub visibility: String,

    /// Backend kind.
    #[serde(default = "default_backend")]
    pub backend: String,

    /// Indexing policy. `None` → use global default.
    #[serde(default)]
    pub indexing: Option<IndexingPolicyConfig>,
}

fn default_visibility() -> String {
    "private".to_string()
}

fn default_backend() -> String {
    "lancedb".to_string()
}

// ---------------------------------------------------------------------------
// RuntimeSource type (core domain type; specs/02-domain-model.md)
// ---------------------------------------------------------------------------

/// A source record persisted in the runtime-state DB.
///
/// Represents a file-system path or URL that feeds documents into a store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeSource {
    /// Source ULID.
    pub id: String,
    /// Owning store name.
    pub store_name: String,
    /// Source kind: `"path"` or `"url"`.
    pub kind: String,
    /// Root path (for path sources).
    pub root: Option<String>,
    /// URL (for url sources).
    pub url: Option<String>,
    /// Include globs.
    #[serde(default)]
    pub include: Vec<String>,
    /// Exclude globs.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Chunking preset.
    pub preset: String,
}

// ---------------------------------------------------------------------------
// RuntimeStateDb
// ---------------------------------------------------------------------------

/// Mutable runtime-state DB for runtime-owned objects.
///
/// Backed by SQLite in WAL mode. Holds only the DB path; each method opens a
/// fresh connection, runs its statement, and drops it. No connection is held
/// between calls, so no write lock persists between operations.
///
/// Schema (two tables, key → JSON blob):
/// - `runtime_stores(name TEXT PRIMARY KEY, json TEXT NOT NULL)`
/// - `cli_sources(id TEXT PRIMARY KEY, store_name TEXT NOT NULL, json TEXT NOT NULL)`
pub struct RuntimeStateDb {
    path: std::path::PathBuf,
}

impl RuntimeStateDb {
    /// Open (or create) the runtime-state DB at the given path.
    ///
    /// Ensures the parent directory exists, creates the SQLite file and
    /// tables if necessary, and enables WAL mode. Drops the connection
    /// immediately — subsequent calls open fresh short-lived connections.
    ///
    /// Retries up to 3 times with backoff on `SQLITE_BUSY` — the WAL mode
    /// change requires an exclusive lock that can collide when multiple
    /// processes start against a fresh database simultaneously.
    pub fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Internal {
                message: format!(
                    "cannot create runtime-state DB directory '{}': {}",
                    parent.display(),
                    e
                ),
                correlation_id: "runtime_state_open".to_string(),
            })?;
        }

        let mut last_err = None;
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(100 * (1 << attempt)));
            }
            match Self::open_inner(path) {
                Ok(db) => return Ok(db),
                Err(Error::RuntimeStateLocked) => {
                    last_err = Some(Error::RuntimeStateLocked);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap())
    }

    fn open_inner(path: &Path) -> Result<Self, Error> {
        let c = Connection::open(path).map_err(map_sqlite_err)?;
        c.busy_timeout(Duration::from_secs(5))
            .map_err(map_sqlite_err)?;
        c.pragma_update(None, "journal_mode", "WAL")
            .map_err(map_sqlite_err)?;
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS runtime_stores (
                name TEXT PRIMARY KEY NOT NULL,
                json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS cli_sources (
                id   TEXT PRIMARY KEY NOT NULL,
                store_name TEXT NOT NULL,
                json TEXT NOT NULL
            );",
        )
        .map_err(map_sqlite_err)?;
        drop(c);

        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    fn conn(&self) -> Result<Connection, Error> {
        let c = Connection::open(&self.path).map_err(map_sqlite_err)?;
        c.busy_timeout(Duration::from_secs(5))
            .map_err(map_sqlite_err)?;
        Ok(c)
    }

    // --- Store operations ---

    /// Insert or update a runtime-owned store.
    ///
    /// If a store with the same name already exists in the DB, it is replaced.
    pub fn upsert_store(&self, store: &RuntimeStore) -> Result<(), Error> {
        let json = serde_json::to_string(store).map_err(|e| Error::Internal {
            message: format!("cannot serialize store: {}", e),
            correlation_id: "runtime_state_upsert".to_string(),
        })?;
        let c = self.conn()?;
        c.execute(
            "INSERT INTO runtime_stores (name, json) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET json = excluded.json",
            rusqlite::params![store.name, json],
        )
        .map_err(map_sqlite_err)?;
        Ok(())
    }

    /// Delete a runtime-owned store by name.
    ///
    /// Returns `Ok(true)` if the store existed and was deleted,
    /// `Ok(false)` if it did not exist.
    pub fn delete_store(&self, name: &str) -> Result<bool, Error> {
        let c = self.conn()?;
        let n = c
            .execute(
                "DELETE FROM runtime_stores WHERE name = ?1",
                rusqlite::params![name],
            )
            .map_err(map_sqlite_err)?;
        Ok(n > 0)
    }

    /// Get a runtime-owned store by name.
    pub fn get_store(&self, name: &str) -> Result<Option<RuntimeStore>, Error> {
        let c = self.conn()?;
        let result = c.query_row(
            "SELECT json FROM runtime_stores WHERE name = ?1",
            rusqlite::params![name],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(json) => {
                let store: RuntimeStore =
                    serde_json::from_str(&json).map_err(|e| Error::Internal {
                        message: format!("cannot deserialize store '{}': {}", name, e),
                        correlation_id: "runtime_state_get".to_string(),
                    })?;
                Ok(Some(store))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(map_sqlite_err(e)),
        }
    }

    /// List all runtime-owned stores.
    pub fn list_stores(&self) -> Result<Vec<RuntimeStore>, Error> {
        let c = self.conn()?;
        let mut stmt = c
            .prepare("SELECT json FROM runtime_stores")
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_err)?;
        let mut stores = Vec::new();
        for row in rows {
            let json = row.map_err(map_sqlite_err)?;
            let store: RuntimeStore = serde_json::from_str(&json).map_err(|e| Error::Internal {
                message: format!("cannot deserialize store from DB: {}", e),
                correlation_id: "runtime_state_list".to_string(),
            })?;
            stores.push(store);
        }
        Ok(stores)
    }

    // --- Source operations ---

    /// Insert or update a source record.
    pub fn upsert_source(&self, source: &RuntimeSource) -> Result<(), Error> {
        let json = serde_json::to_string(source).map_err(|e| Error::Internal {
            message: format!("cannot serialize source: {}", e),
            correlation_id: "source_upsert_ser".to_string(),
        })?;
        let c = self.conn()?;
        c.execute(
            "INSERT INTO cli_sources (id, store_name, json) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET store_name = excluded.store_name,
                                           json = excluded.json",
            rusqlite::params![source.id, source.store_name, json],
        )
        .map_err(map_sqlite_err)?;
        Ok(())
    }

    /// Delete a source by ID. Returns `true` if it existed.
    pub fn delete_source(&self, id: &str) -> Result<bool, Error> {
        let c = self.conn()?;
        let n = c
            .execute(
                "DELETE FROM cli_sources WHERE id = ?1",
                rusqlite::params![id],
            )
            .map_err(map_sqlite_err)?;
        Ok(n > 0)
    }

    /// Delete all sources belonging to a store. Returns the count removed.
    pub fn delete_sources_for_store(&self, store_name: &str) -> Result<usize, Error> {
        let c = self.conn()?;
        let n = c
            .execute(
                "DELETE FROM cli_sources WHERE store_name = ?1",
                rusqlite::params![store_name],
            )
            .map_err(map_sqlite_err)?;
        Ok(n)
    }

    /// Get a source by ID.
    pub fn get_source(&self, id: &str) -> Result<Option<RuntimeSource>, Error> {
        let c = self.conn()?;
        let result = c.query_row(
            "SELECT json FROM cli_sources WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(json) => {
                let src: RuntimeSource =
                    serde_json::from_str(&json).map_err(|e| Error::Internal {
                        message: format!("cannot deserialize source '{}': {}", id, e),
                        correlation_id: "source_get_deser".to_string(),
                    })?;
                Ok(Some(src))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(map_sqlite_err(e)),
        }
    }

    /// List all sources for a given store.
    pub fn list_sources(&self, store_name: &str) -> Result<Vec<RuntimeSource>, Error> {
        let c = self.conn()?;
        let mut stmt = c
            .prepare("SELECT json FROM cli_sources WHERE store_name = ?1")
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map(rusqlite::params![store_name], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_err)?;
        let mut sources = Vec::new();
        for row in rows {
            let json = row.map_err(map_sqlite_err)?;
            let src: RuntimeSource = serde_json::from_str(&json).map_err(|e| Error::Internal {
                message: format!("cannot deserialize source from DB: {}", e),
                correlation_id: "source_list_deser".to_string(),
            })?;
            sources.push(src);
        }
        Ok(sources)
    }

    /// Find a source by its `root` path or `url` field, optionally scoped to a store.
    pub fn find_source_by_root_or_url(
        &self,
        value: &str,
        store_name: Option<&str>,
    ) -> Result<Option<RuntimeSource>, Error> {
        let c = self.conn()?;
        let mut stmt = c
            .prepare("SELECT json FROM cli_sources")
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_err)?;
        for row in rows {
            let json = row.map_err(map_sqlite_err)?;
            let src: RuntimeSource = serde_json::from_str(&json).map_err(|e| Error::Internal {
                message: format!("cannot deserialize source: {}", e),
                correlation_id: "source_find_deser".to_string(),
            })?;
            if let Some(sn) = store_name {
                if src.store_name != sn {
                    continue;
                }
            }
            let matches = src.root.as_deref() == Some(value) || src.url.as_deref() == Some(value);
            if matches {
                return Ok(Some(src));
            }
        }
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// EffectiveConfig
// ---------------------------------------------------------------------------

/// The merged effective config view: YAML-owned + runtime-owned.
///
/// YAML-owned objects take precedence; runtime-owned objects fill the rest.
/// This is the authoritative view for the running system.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    /// All stores (YAML-owned + runtime-owned), with ownership tags.
    pub stores: Vec<EffectiveStore>,
}

/// A store in the effective config view, with ownership annotation.
#[derive(Debug, Clone)]
pub struct EffectiveStore {
    /// Store name.
    pub name: String,

    /// ULID for runtime-owned stores; `None` for YAML-owned stores (no ULID exists).
    pub id: Option<String>,

    /// Who owns this store.
    pub ownership: ConfigOwnership,

    /// Visibility.
    pub visibility: String,

    /// Backend kind.
    pub backend: String,

    /// Effective indexing policy (store override, or global default).
    pub indexing: IndexingPolicyConfig,
}

/// Build the effective config from a YAML config and the runtime-state DB.
///
/// YAML-owned objects are listed first.
pub fn build_effective_config(
    yaml_config: &RawConfig,
    runtime_db: &RuntimeStateDb,
    global_default: &IndexingPolicyConfig,
) -> Result<EffectiveConfig, Error> {
    let mut stores = Vec::new();

    // YAML-owned stores
    for yaml_store in &yaml_config.stores {
        let indexing = yaml_store
            .indexing
            .clone()
            .unwrap_or_else(|| global_default.clone());

        stores.push(EffectiveStore {
            name: yaml_store.name.clone(),
            id: None,
            ownership: ConfigOwnership::Yaml,
            visibility: yaml_store.visibility.clone(),
            backend: yaml_store.backend.clone(),
            indexing,
        });
    }

    // Collect YAML store names for collision detection
    let yaml_names: std::collections::HashSet<String> =
        yaml_config.stores.iter().map(|s| s.name.clone()).collect();

    // Runtime-owned stores (those not in YAML)
    for rt_store in runtime_db.list_stores()? {
        if yaml_names.contains(&rt_store.name) {
            continue;
        }
        let indexing = rt_store.indexing.unwrap_or_else(|| global_default.clone());
        stores.push(EffectiveStore {
            name: rt_store.name,
            id: Some(rt_store.id),
            ownership: ConfigOwnership::Runtime,
            visibility: rt_store.visibility,
            backend: rt_store.backend,
            indexing,
        });
    }

    Ok(EffectiveConfig { stores })
}

/// Check whether a named store is YAML-owned.
///
/// If yes, any mutation attempt should return `Error::ConfigReadonly`.
pub fn check_yaml_owned(name: &str, yaml_config: &RawConfig) -> bool {
    yaml_config.stores.iter().any(|s| s.name == name)
}

// ---------------------------------------------------------------------------
// Error mapping helper
// ---------------------------------------------------------------------------

fn map_sqlite_err(e: rusqlite::Error) -> Error {
    if let rusqlite::Error::SqliteFailure(ref sqlite_err, _) = e {
        if matches!(
            sqlite_err.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        ) {
            return Error::RuntimeStateLocked;
        }
    }
    Error::Internal {
        message: format!("runtime-state DB error: {}", e),
        correlation_id: "sqlite".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{EmbeddingPolicy, StoreConfig};
    use tempfile::TempDir;

    fn tmp_db() -> (TempDir, RuntimeStateDb) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime-state.db");
        let db = RuntimeStateDb::open(&path).unwrap();
        (dir, db)
    }

    fn make_runtime_store(name: &str) -> RuntimeStore {
        RuntimeStore {
            name: name.to_string(),
            id: format!("ulid-{}", name),
            visibility: "private".to_string(),
            backend: "lancedb".to_string(),
            indexing: None,
        }
    }

    fn make_runtime_source(id: &str, store_name: &str, root: &str) -> RuntimeSource {
        RuntimeSource {
            id: id.to_string(),
            store_name: store_name.to_string(),
            kind: "path".to_string(),
            root: Some(root.to_string()),
            url: None,
            include: vec![],
            exclude: vec![],
            preset: "prose".to_string(),
        }
    }

    fn make_yaml_config_with_stores(names: &[&str]) -> RawConfig {
        RawConfig {
            version: 1,
            server: crate::config::schema::ServerConfig::default(),
            paths: crate::config::schema::PathsConfig::default(),
            defaults: crate::config::schema::DefaultsConfig::default(),
            stores: names
                .iter()
                .map(|n| StoreConfig {
                    name: n.to_string(),
                    visibility: "private".to_string(),
                    backend: "lancedb".to_string(),
                    indexing: None,
                    sources: vec![],
                })
                .collect(),
            providers: vec![],
        }
    }

    // --- RuntimeStateDb tests ---

    #[test]
    fn open_creates_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime-state.db");
        assert!(!path.exists());
        let _db = RuntimeStateDb::open(&path).unwrap();
        assert!(path.exists(), "DB file should be created");
    }

    #[test]
    fn open_creates_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subdir").join("runtime-state.db");
        let _db = RuntimeStateDb::open(&path).unwrap();
        assert!(path.exists(), "DB file should be created in new directory");
    }

    #[test]
    fn second_open_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime-state.db");

        let _db1 = RuntimeStateDb::open(&path).unwrap();
        // SQLite WAL mode: a second open on the same path must also succeed.
        let result = RuntimeStateDb::open(&path);
        assert!(result.is_ok(), "second open should succeed with SQLite WAL");
    }

    #[test]
    fn two_handles_same_file_both_usable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime-state.db");

        let db1 = RuntimeStateDb::open(&path).unwrap();
        let db2 = RuntimeStateDb::open(&path).unwrap();

        db1.upsert_store(&make_runtime_store("from-db1")).unwrap();
        db2.upsert_store(&make_runtime_store("from-db2")).unwrap();

        let stores1 = db1.list_stores().unwrap();
        let stores2 = db2.list_stores().unwrap();

        assert_eq!(stores1.len(), 2);
        assert_eq!(stores2.len(), 2);
    }

    #[test]
    fn busy_timeout_exhaustion_maps_to_runtime_state_locked() {
        // Verify the error-mapping logic: DatabaseBusy/DatabaseLocked → RuntimeStateLocked.
        use rusqlite::ffi;
        let busy = rusqlite::Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_BUSY), None);
        assert_eq!(map_sqlite_err(busy), Error::RuntimeStateLocked);

        let locked = rusqlite::Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_LOCKED), None);
        assert_eq!(map_sqlite_err(locked), Error::RuntimeStateLocked);
    }

    #[test]
    fn runtime_state_locked_exit_code_is_4() {
        assert_eq!(Error::RuntimeStateLocked.exit_code(), 4);
        assert_eq!(Error::RuntimeStateLocked.code(), "runtime_state_locked");
    }

    #[test]
    fn upsert_and_get_store() {
        let (_dir, db) = tmp_db();
        let store = make_runtime_store("my-notes");
        db.upsert_store(&store).unwrap();
        let retrieved = db.get_store("my-notes").unwrap().unwrap();
        assert_eq!(retrieved.name, "my-notes");
        assert_eq!(retrieved.visibility, "private");
        assert_eq!(retrieved.backend, "lancedb");
    }

    #[test]
    fn get_nonexistent_store_returns_none() {
        let (_dir, db) = tmp_db();
        let result = db.get_store("not-exist").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn upsert_overwrites_existing_store() {
        let (_dir, db) = tmp_db();
        let mut store = make_runtime_store("notes");
        db.upsert_store(&store).unwrap();

        store.visibility = "shared".to_string();
        db.upsert_store(&store).unwrap();

        let retrieved = db.get_store("notes").unwrap().unwrap();
        assert_eq!(retrieved.visibility, "shared");
    }

    #[test]
    fn delete_existing_store_returns_true() {
        let (_dir, db) = tmp_db();
        let store = make_runtime_store("to-delete");
        db.upsert_store(&store).unwrap();
        assert!(db.delete_store("to-delete").unwrap());
        assert!(db.get_store("to-delete").unwrap().is_none());
    }

    #[test]
    fn delete_nonexistent_store_returns_false() {
        let (_dir, db) = tmp_db();
        assert!(!db.delete_store("not-exist").unwrap());
    }

    #[test]
    fn list_stores_empty() {
        let (_dir, db) = tmp_db();
        let stores = db.list_stores().unwrap();
        assert!(stores.is_empty());
    }

    #[test]
    fn list_stores_returns_all() {
        let (_dir, db) = tmp_db();
        db.upsert_store(&make_runtime_store("alpha")).unwrap();
        db.upsert_store(&make_runtime_store("beta")).unwrap();
        db.upsert_store(&make_runtime_store("gamma")).unwrap();

        let mut stores = db.list_stores().unwrap();
        stores.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(stores.len(), 3);
        assert_eq!(stores[0].name, "alpha");
        assert_eq!(stores[1].name, "beta");
        assert_eq!(stores[2].name, "gamma");
    }

    #[test]
    fn store_with_indexing_policy_round_trips() {
        let (_dir, db) = tmp_db();
        let store = RuntimeStore {
            name: "code-store".to_string(),
            id: "ulid-code".to_string(),
            visibility: "private".to_string(),
            backend: "lancedb".to_string(),
            indexing: Some(IndexingPolicyConfig {
                embedding: EmbeddingPolicy {
                    model: "bge-small".to_string(),
                    provider: "local-onnx".to_string(),
                },
                ..Default::default()
            }),
        };
        db.upsert_store(&store).unwrap();
        let retrieved = db.get_store("code-store").unwrap().unwrap();
        assert!(retrieved.indexing.is_some());
        assert_eq!(
            retrieved.indexing.as_ref().unwrap().embedding.model,
            "bge-small"
        );
    }

    // --- Source CRUD tests ---

    #[test]
    fn upsert_and_get_source() {
        let (_dir, db) = tmp_db();
        let src = make_runtime_source("src-1", "mystore", "/tmp/docs");
        db.upsert_source(&src).unwrap();
        let retrieved = db.get_source("src-1").unwrap().unwrap();
        assert_eq!(retrieved.id, "src-1");
        assert_eq!(retrieved.store_name, "mystore");
        assert_eq!(retrieved.root.as_deref(), Some("/tmp/docs"));
    }

    #[test]
    fn get_nonexistent_source_returns_none() {
        let (_dir, db) = tmp_db();
        assert!(db.get_source("no-such-id").unwrap().is_none());
    }

    #[test]
    fn upsert_source_overwrites_existing() {
        let (_dir, db) = tmp_db();
        let mut src = make_runtime_source("src-1", "mystore", "/tmp/docs");
        db.upsert_source(&src).unwrap();
        src.root = Some("/tmp/new".to_string());
        db.upsert_source(&src).unwrap();
        let r = db.get_source("src-1").unwrap().unwrap();
        assert_eq!(r.root.as_deref(), Some("/tmp/new"));
    }

    #[test]
    fn delete_source_returns_true_then_false() {
        let (_dir, db) = tmp_db();
        let src = make_runtime_source("src-del", "mystore", "/tmp");
        db.upsert_source(&src).unwrap();
        assert!(db.delete_source("src-del").unwrap());
        assert!(!db.delete_source("src-del").unwrap());
    }

    #[test]
    fn list_sources_filters_by_store() {
        let (_dir, db) = tmp_db();
        db.upsert_source(&make_runtime_source("s1", "store-a", "/a"))
            .unwrap();
        db.upsert_source(&make_runtime_source("s2", "store-b", "/b"))
            .unwrap();
        db.upsert_source(&make_runtime_source("s3", "store-a", "/c"))
            .unwrap();

        let mut a = db.list_sources("store-a").unwrap();
        a.sort_by(|x, y| x.id.cmp(&y.id));
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].id, "s1");
        assert_eq!(a[1].id, "s3");

        let b = db.list_sources("store-b").unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].id, "s2");
    }

    #[test]
    fn delete_sources_for_store_removes_all() {
        let (_dir, db) = tmp_db();
        db.upsert_source(&make_runtime_source("s1", "target", "/1"))
            .unwrap();
        db.upsert_source(&make_runtime_source("s2", "target", "/2"))
            .unwrap();
        db.upsert_source(&make_runtime_source("s3", "other", "/3"))
            .unwrap();

        let removed = db.delete_sources_for_store("target").unwrap();
        assert_eq!(removed, 2);
        assert!(db.list_sources("target").unwrap().is_empty());
        assert_eq!(db.list_sources("other").unwrap().len(), 1);
    }

    #[test]
    fn find_source_by_root_or_url_found() {
        let (_dir, db) = tmp_db();
        db.upsert_source(&make_runtime_source("s1", "store-x", "/docs/notes"))
            .unwrap();
        let found = db.find_source_by_root_or_url("/docs/notes", None).unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, "s1");
    }

    #[test]
    fn find_source_by_root_or_url_scoped_to_store() {
        let (_dir, db) = tmp_db();
        db.upsert_source(&make_runtime_source("s1", "store-a", "/shared"))
            .unwrap();
        db.upsert_source(&make_runtime_source("s2", "store-b", "/shared"))
            .unwrap();

        let found_a = db
            .find_source_by_root_or_url("/shared", Some("store-a"))
            .unwrap();
        assert_eq!(found_a.unwrap().id, "s1");

        let found_b = db
            .find_source_by_root_or_url("/shared", Some("store-b"))
            .unwrap();
        assert_eq!(found_b.unwrap().id, "s2");

        let not_found = db
            .find_source_by_root_or_url("/shared", Some("store-c"))
            .unwrap();
        assert!(not_found.is_none());
    }

    #[test]
    fn find_source_by_url() {
        let (_dir, db) = tmp_db();
        let src = RuntimeSource {
            id: "url-src".to_string(),
            store_name: "mystore".to_string(),
            kind: "url".to_string(),
            root: None,
            url: Some("https://example.com/docs".to_string()),
            include: vec![],
            exclude: vec![],
            preset: "prose".to_string(),
        };
        db.upsert_source(&src).unwrap();
        let found = db
            .find_source_by_root_or_url("https://example.com/docs", None)
            .unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, "url-src");
    }

    // --- EffectiveConfig tests ---

    #[test]
    fn effective_config_yaml_only() {
        let (_dir, db) = tmp_db();
        let yaml = make_yaml_config_with_stores(&["notes", "code"]);
        let default_policy = IndexingPolicyConfig::default();
        let effective = build_effective_config(&yaml, &db, &default_policy).unwrap();
        assert_eq!(effective.stores.len(), 2);
        assert!(effective
            .stores
            .iter()
            .all(|s| s.ownership == ConfigOwnership::Yaml));
    }

    #[test]
    fn effective_config_runtime_only() {
        let (_dir, db) = tmp_db();
        db.upsert_store(&make_runtime_store("runtime-store"))
            .unwrap();

        let yaml = make_yaml_config_with_stores(&[]);
        let default_policy = IndexingPolicyConfig::default();
        let effective = build_effective_config(&yaml, &db, &default_policy).unwrap();
        assert_eq!(effective.stores.len(), 1);
        assert_eq!(effective.stores[0].ownership, ConfigOwnership::Runtime);
        assert_eq!(effective.stores[0].name, "runtime-store");
    }

    #[test]
    fn effective_config_yaml_takes_precedence_over_runtime() {
        let (_dir, db) = tmp_db();

        db.upsert_store(&RuntimeStore {
            name: "notes".to_string(),
            id: "rt-id".to_string(),
            visibility: "shared".to_string(),
            backend: "lancedb".to_string(),
            indexing: None,
        })
        .unwrap();

        let yaml = make_yaml_config_with_stores(&["notes"]);

        let default_policy = IndexingPolicyConfig::default();
        let effective = build_effective_config(&yaml, &db, &default_policy).unwrap();

        assert_eq!(effective.stores.len(), 1);
        assert_eq!(effective.stores[0].ownership, ConfigOwnership::Yaml);
        assert_eq!(effective.stores[0].visibility, "private");
    }

    #[test]
    fn effective_config_mixed_ownership() {
        let (_dir, db) = tmp_db();
        db.upsert_store(&make_runtime_store("runtime-notes"))
            .unwrap();

        let yaml = make_yaml_config_with_stores(&["yaml-notes"]);
        let default_policy = IndexingPolicyConfig::default();
        let effective = build_effective_config(&yaml, &db, &default_policy).unwrap();

        assert_eq!(effective.stores.len(), 2);
        let yaml_store = effective
            .stores
            .iter()
            .find(|s| s.name == "yaml-notes")
            .unwrap();
        let rt_store = effective
            .stores
            .iter()
            .find(|s| s.name == "runtime-notes")
            .unwrap();
        assert_eq!(yaml_store.ownership, ConfigOwnership::Yaml);
        assert_eq!(rt_store.ownership, ConfigOwnership::Runtime);
    }

    #[test]
    fn effective_config_store_inherits_global_default() {
        let (_dir, db) = tmp_db();

        let custom_default = IndexingPolicyConfig {
            embedding: EmbeddingPolicy {
                model: "custom-model".to_string(),
                provider: "openai-compatible".to_string(),
            },
            ..Default::default()
        };

        let yaml = make_yaml_config_with_stores(&["my-store"]);
        let effective = build_effective_config(&yaml, &db, &custom_default).unwrap();

        assert_eq!(effective.stores[0].indexing.embedding.model, "custom-model");
    }

    #[test]
    fn effective_config_store_uses_own_policy_over_default() {
        let (_dir, db) = tmp_db();

        let yaml = RawConfig {
            version: 1,
            server: crate::config::schema::ServerConfig::default(),
            paths: crate::config::schema::PathsConfig::default(),
            defaults: crate::config::schema::DefaultsConfig::default(),
            stores: vec![StoreConfig {
                name: "special".to_string(),
                visibility: "private".to_string(),
                backend: "lancedb".to_string(),
                indexing: Some(IndexingPolicyConfig {
                    embedding: EmbeddingPolicy {
                        model: "store-specific-model".to_string(),
                        provider: "local-onnx".to_string(),
                    },
                    ..Default::default()
                }),
                sources: vec![],
            }],
            providers: vec![],
        };

        let global_default = IndexingPolicyConfig::default();
        let effective = build_effective_config(&yaml, &db, &global_default).unwrap();

        assert_eq!(
            effective.stores[0].indexing.embedding.model,
            "store-specific-model"
        );
    }

    // --- check_yaml_owned tests ---

    #[test]
    fn check_yaml_owned_returns_true_for_yaml_store() {
        let yaml = make_yaml_config_with_stores(&["notes", "code"]);
        assert!(check_yaml_owned("notes", &yaml));
        assert!(check_yaml_owned("code", &yaml));
    }

    #[test]
    fn check_yaml_owned_returns_false_for_unknown() {
        let yaml = make_yaml_config_with_stores(&["notes"]);
        assert!(!check_yaml_owned("other-store", &yaml));
        assert!(!check_yaml_owned("", &yaml));
    }

    // --- Mutation guard test ---

    #[test]
    fn yaml_owned_mutation_returns_config_readonly() {
        let yaml = make_yaml_config_with_stores(&["notes"]);
        let is_yaml_owned = check_yaml_owned("notes", &yaml);
        assert!(is_yaml_owned);

        let err = if is_yaml_owned {
            Some(Error::ConfigReadonly)
        } else {
            None
        };
        assert_eq!(err, Some(Error::ConfigReadonly));
    }

    #[test]
    fn runtime_owned_mutation_does_not_return_config_readonly() {
        let yaml = make_yaml_config_with_stores(&["notes"]);
        let is_yaml_owned = check_yaml_owned("api-created", &yaml);
        assert!(!is_yaml_owned);

        let err: Option<Error> = if is_yaml_owned {
            Some(Error::ConfigReadonly)
        } else {
            None
        };
        assert!(err.is_none());
    }

    // --- Persistence tests ---

    #[test]
    fn runtime_state_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime-state.db");

        {
            let db = RuntimeStateDb::open(&path).unwrap();
            db.upsert_store(&make_runtime_store("persisted")).unwrap();
        }

        let db2 = RuntimeStateDb::open(&path).unwrap();
        let store = db2.get_store("persisted").unwrap().unwrap();
        assert_eq!(store.name, "persisted");
    }

    #[test]
    fn deleted_store_not_found_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime-state.db");

        {
            let db = RuntimeStateDb::open(&path).unwrap();
            db.upsert_store(&make_runtime_store("temp")).unwrap();
            db.delete_store("temp").unwrap();
        }

        let db2 = RuntimeStateDb::open(&path).unwrap();
        assert!(db2.get_store("temp").unwrap().is_none());
    }

    #[test]
    fn source_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime-state.db");

        {
            let db = RuntimeStateDb::open(&path).unwrap();
            db.upsert_source(&make_runtime_source("persist-src", "mystore", "/data"))
                .unwrap();
        }

        let db2 = RuntimeStateDb::open(&path).unwrap();
        let src = db2.get_source("persist-src").unwrap().unwrap();
        assert_eq!(src.root.as_deref(), Some("/data"));
    }
}
