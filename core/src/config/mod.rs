//! Configuration for localdb.
//!
//! See specs/03-config.md for full specification.

pub mod loader;
pub mod platform;
pub mod policy;
pub mod refresh;
pub mod schema;

pub use loader::{
    load_config, load_config_from_str, refuse_legacy_layout, ConfigLoader, LoadOptions,
};
pub use platform::PlatformPaths;
pub use policy::compute_policy_version;
pub use refresh::validate_refresh_interval;
pub use schema::{
    ChunkingPolicy, DefaultsConfig, EmbeddingPolicy, IndexingPolicyConfig, PathsConfig,
    ProviderConfig, RawConfig, ServerConfig,
};
