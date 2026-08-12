//! Configuration for localdb.
//!
//! See specs/03-config.md for full specification.

pub mod feed;
pub mod jsonschema;
pub mod loader;
pub mod platform;
pub mod policy;
pub mod refresh;
pub mod schema;
pub mod template;

pub use feed::validate_max_entries;
pub use jsonschema::{generate_router_schema, SCHEMA_URL};
pub use loader::{
    load_config, load_config_from_str, refuse_legacy_layout, resolve_config_path, ConfigLoader,
    LoadOptions,
};
pub use platform::PlatformPaths;
pub use policy::compute_policy_version;
pub use refresh::validate_refresh_interval;
pub use schema::{
    ChunkingPolicy, DefaultsConfig, EmbeddingPolicy, IndexingPolicyConfig, PathsConfig,
    ProviderConfig, RawConfig, ServerConfig,
};
pub use template::render_default_config_template;
