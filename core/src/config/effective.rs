use serde::{Deserialize, Serialize};

use crate::config::schema::{IndexingPolicyConfig, RawConfig};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigOwnership {
    Yaml,
    Runtime,
}

#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub stores: Vec<EffectiveStore>,
}

#[derive(Debug, Clone)]
pub struct EffectiveStore {
    pub name: String,
    pub id: Option<String>,
    pub ownership: ConfigOwnership,
    pub visibility: String,
    pub backend: String,
    pub indexing: IndexingPolicyConfig,
}

pub fn check_yaml_owned(name: &str, yaml_config: &RawConfig) -> bool {
    yaml_config.stores.iter().any(|s| s.name == name)
}
