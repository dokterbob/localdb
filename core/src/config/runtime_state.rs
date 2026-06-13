//! Runtime-state DB backed by redb.
//!
//! Stores runtime-owned objects (stores/sources added via API/CLI)
//! separately from the YAML config. Never touches the YAML file.
//!
//! Ownership model (specs/03-config.md §3):
//! - YAML-owned: object appears in the YAML config (matched by name).
//!   Mutations via API return `config_readonly`.
//! - Runtime-owned: object was created via API/CLI. Lives in the DB.
//!
//! The `EffectiveConfig` merges both views: YAML-owned objects take precedence
//! over runtime-owned objects with the same name.

use std::path::Path;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
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
// Redb table definitions
// ---------------------------------------------------------------------------

/// Runtime-owned stores: name → JSON-serialized `RuntimeStore`.
const RUNTIME_STORES_TABLE: TableDefinition<&str, &str> = TableDefinition::new("runtime_stores");

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
// RuntimeStateDb
// ---------------------------------------------------------------------------

/// Mutable runtime-state DB for runtime-owned objects.
///
/// Backed by redb (embedded, transactional).
/// All writes are transactional; reads are consistent.
pub struct RuntimeStateDb {
    db: Database,
}

impl RuntimeStateDb {
    /// Open (or create) the runtime-state DB at the given path.
    pub fn open(path: &Path) -> Result<Self, Error> {
        // Ensure the parent directory exists
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

        let db = Database::create(path).map_err(|e| Error::Internal {
            message: format!(
                "cannot open runtime-state DB at '{}': {}",
                path.display(),
                e
            ),
            correlation_id: "runtime_state_open".to_string(),
        })?;

        // Ensure tables exist
        {
            let write_txn = db.begin_write().map_err(map_redb_err)?;
            write_txn
                .open_table(RUNTIME_STORES_TABLE)
                .map_err(map_redb_err)?;
            write_txn.commit().map_err(map_redb_err)?;
        }

        Ok(Self { db })
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

        let write_txn = self.db.begin_write().map_err(map_redb_err)?;
        {
            let mut table = write_txn
                .open_table(RUNTIME_STORES_TABLE)
                .map_err(map_redb_err)?;
            table
                .insert(store.name.as_str(), json.as_str())
                .map_err(map_redb_err)?;
        }
        write_txn.commit().map_err(map_redb_err)?;
        Ok(())
    }

    /// Delete a runtime-owned store by name.
    ///
    /// Returns `Ok(true)` if the store existed and was deleted,
    /// `Ok(false)` if it did not exist.
    pub fn delete_store(&self, name: &str) -> Result<bool, Error> {
        let write_txn = self.db.begin_write().map_err(map_redb_err)?;
        let deleted = {
            let mut table = write_txn
                .open_table(RUNTIME_STORES_TABLE)
                .map_err(map_redb_err)?;
            let x = table.remove(name).map_err(map_redb_err)?.is_some();
            x
        };
        write_txn.commit().map_err(map_redb_err)?;
        Ok(deleted)
    }

    /// Get a runtime-owned store by name.
    pub fn get_store(&self, name: &str) -> Result<Option<RuntimeStore>, Error> {
        let read_txn = self.db.begin_read().map_err(map_redb_err)?;
        let table = read_txn
            .open_table(RUNTIME_STORES_TABLE)
            .map_err(map_redb_err)?;
        match table.get(name).map_err(map_redb_err)? {
            None => Ok(None),
            Some(guard) => {
                let json = guard.value();
                let store: RuntimeStore =
                    serde_json::from_str(json).map_err(|e| Error::Internal {
                        message: format!("cannot deserialize store '{}': {}", name, e),
                        correlation_id: "runtime_state_get".to_string(),
                    })?;
                Ok(Some(store))
            }
        }
    }

    /// List all runtime-owned stores.
    pub fn list_stores(&self) -> Result<Vec<RuntimeStore>, Error> {
        let read_txn = self.db.begin_read().map_err(map_redb_err)?;
        let table = read_txn
            .open_table(RUNTIME_STORES_TABLE)
            .map_err(map_redb_err)?;

        let mut stores = Vec::new();
        for entry in table.iter().map_err(map_redb_err)? {
            let (_, value) = entry.map_err(map_redb_err)?;
            let json = value.value();
            let store: RuntimeStore = serde_json::from_str(json).map_err(|e| Error::Internal {
                message: format!("cannot deserialize store from DB: {}", e),
                correlation_id: "runtime_state_list".to_string(),
            })?;
            stores.push(store);
        }
        Ok(stores)
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
            // YAML wins; skip the runtime copy
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

fn map_redb_err(e: impl std::fmt::Display) -> Error {
    Error::Internal {
        message: format!("runtime-state DB error: {}", e),
        correlation_id: "redb".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{ChunkingPolicy, EmbeddingPolicy, StoreConfig};
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn tmp_db() -> (TempDir, RuntimeStateDb) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime-state.redb");
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
        let path = dir.path().join("runtime-state.redb");
        assert!(!path.exists());
        let _db = RuntimeStateDb::open(&path).unwrap();
        assert!(path.exists(), "DB file should be created");
    }

    #[test]
    fn open_creates_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subdir").join("runtime-state.redb");
        let _db = RuntimeStateDb::open(&path).unwrap();
        assert!(path.exists(), "DB file should be created in new directory");
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
                chunking: ChunkingPolicy {
                    preset_overrides: HashMap::new(),
                },
                embedding: EmbeddingPolicy {
                    model: "bge-small".to_string(),
                    provider: "local-onnx".to_string(),
                },
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

        // Same name in both YAML and runtime — YAML wins
        db.upsert_store(&RuntimeStore {
            name: "notes".to_string(),
            id: "rt-id".to_string(),
            visibility: "shared".to_string(), // runtime says shared
            backend: "lancedb".to_string(),
            indexing: None,
        })
        .unwrap();

        let yaml = make_yaml_config_with_stores(&["notes"]); // YAML says private

        let default_policy = IndexingPolicyConfig::default();
        let effective = build_effective_config(&yaml, &db, &default_policy).unwrap();

        // Only one store (no duplicate), and it's YAML-owned
        assert_eq!(effective.stores.len(), 1);
        assert_eq!(effective.stores[0].ownership, ConfigOwnership::Yaml);
        assert_eq!(effective.stores[0].visibility, "private"); // YAML value wins
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
            chunking: ChunkingPolicy {
                preset_overrides: HashMap::new(),
            },
            embedding: EmbeddingPolicy {
                model: "custom-model".to_string(),
                provider: "openai-compatible".to_string(),
            },
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
                    chunking: ChunkingPolicy {
                        preset_overrides: HashMap::new(),
                    },
                    embedding: EmbeddingPolicy {
                        model: "store-specific-model".to_string(),
                        provider: "local-onnx".to_string(),
                    },
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
        // This tests the business rule: if a store is YAML-owned,
        // any mutation attempt (simulated here) should return config_readonly.
        let yaml = make_yaml_config_with_stores(&["notes"]);
        let is_yaml_owned = check_yaml_owned("notes", &yaml);
        assert!(is_yaml_owned);

        // Simulate what an API handler would do:
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

    // --- Persistence test ---

    #[test]
    fn runtime_state_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime-state.redb");

        {
            let db = RuntimeStateDb::open(&path).unwrap();
            db.upsert_store(&make_runtime_store("persisted")).unwrap();
        }

        // Reopen
        let db2 = RuntimeStateDb::open(&path).unwrap();
        let store = db2.get_store("persisted").unwrap().unwrap();
        assert_eq!(store.name, "persisted");
    }

    #[test]
    fn deleted_store_not_found_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime-state.redb");

        {
            let db = RuntimeStateDb::open(&path).unwrap();
            db.upsert_store(&make_runtime_store("temp")).unwrap();
            db.delete_store("temp").unwrap();
        }

        let db2 = RuntimeStateDb::open(&path).unwrap();
        assert!(db2.get_store("temp").unwrap().is_none());
    }
}
