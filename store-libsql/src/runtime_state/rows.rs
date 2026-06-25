//! Public row DTOs returned by `RuntimeStateApi`.

use serde::{Deserialize, Serialize};

use localdb_core::types::{SourceKind, StoreVisibility};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoreRow {
    pub id: String,
    pub name: String,
    pub visibility: StoreVisibility,
    pub backend: String,
    pub indexing_policy: String,
    pub policy_version: String,
    pub acl: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceRow {
    pub id: String,
    pub store_id: String,
    pub kind: SourceKind,
    pub root: Option<String>,
    pub url: Option<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub preset: String,
    pub refresh: Option<String>,
    pub created_at: String,
}
