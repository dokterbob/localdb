//! Configuration for localdb.
//!
//! Split model per specs/03-config.md §3:
//! - **YAML config**: user-owned declarative bootstrap config; never rewritten by machine.
//! - **Runtime state DB**: mutable runtime-owned objects (stores/sources added via API/CLI).
//!
//! See specs/03-config.md for full specification.

pub mod loader;
pub mod platform;
pub mod policy;
pub mod runtime_state;
pub mod schema;

pub use loader::{load_config, load_config_from_str, ConfigLoader, LoadOptions};
pub use platform::PlatformPaths;
pub use policy::compute_policy_version;
pub use runtime_state::{
    ConfigOwnership, EffectiveConfig, EffectiveStore, RuntimeSource, RuntimeStateDb, RuntimeStore,
};
pub use schema::{
    ChunkingPolicy, DefaultsConfig, EmbeddingPolicy, IndexingPolicyConfig, PathsConfig,
    ProviderConfig, RawConfig, ServerConfig, SourceConfig, StoreConfig,
};
