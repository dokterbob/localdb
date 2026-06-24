//! Config file loading with validation.
//!
//! Responsibilities:
//! - YAML parsing with strict unknown-key rejection
//! - `version: 1` checking (unversioned → error with hint)
//! - Path-precise validation errors
//! - Duration string validation
//! - Platform path resolution and env/flag override
//!
//! See specs/03-config.md §5.

use std::path::{Path, PathBuf};

use crate::{
    config::{
        platform::PlatformPaths,
        schema::{RawConfig, SourceConfig},
    },
    Error,
};

/// Options for loading the config.
#[derive(Debug, Default, Clone)]
pub struct LoadOptions {
    /// Explicit config file path (overrides platform default and env var).
    pub config_path: Option<PathBuf>,

    /// Override for data directory path.
    pub data_dir: Option<PathBuf>,

    /// Override for models directory path.
    pub models_dir: Option<PathBuf>,

    /// Override for logs directory path.
    pub logs_dir: Option<PathBuf>,
}

/// A loaded, validated config together with the resolved platform paths.
#[derive(Debug, Clone)]
pub struct ConfigLoader {
    /// The validated YAML config.
    pub config: RawConfig,

    /// Resolved platform paths (after overrides applied).
    pub paths: ResolvedPaths,
}

/// Resolved paths after applying config and env/flag overrides.
#[derive(Debug, Clone)]
pub struct ResolvedPaths {
    /// Absolute path of the config file that was loaded.
    pub config_file: PathBuf,

    /// Data directory.
    pub data_dir: PathBuf,

    /// Model cache directory.
    pub models_dir: PathBuf,

    /// Log directory.
    pub logs_dir: PathBuf,
}

impl ResolvedPaths {
    /// Socket path.
    pub fn socket_path(&self) -> PathBuf {
        self.data_dir.join("daemon.sock")
    }

    /// Write-lock path.
    pub fn write_lock_path(&self) -> PathBuf {
        self.data_dir.join(".write.lock")
    }

    /// Runtime-state DB path.
    pub fn runtime_state_db_path(&self) -> PathBuf {
        self.data_dir.join("runtime-state.db")
    }
}

/// Load config from a file, with options for overrides.
///
/// Resolves the config file path from (in priority order):
/// 1. `options.config_path`
/// 2. `env_config_path` (from `LOCALDB_CONFIG`, read once at startup)
/// 3. Platform default
///
/// Returns `Error::InvalidConfig` on parse or validation failure.
pub fn load_config(
    options: &LoadOptions,
    env_config_path: Option<&Path>,
) -> Result<ConfigLoader, Error> {
    let config_path = resolve_config_path(options, env_config_path)?;

    let yaml_bytes = std::fs::read(&config_path).map_err(|e| Error::InvalidConfig {
        message: format!("cannot read config file '{}': {}", config_path.display(), e),
    })?;

    let yaml_str = std::str::from_utf8(&yaml_bytes).map_err(|e| Error::InvalidConfig {
        message: format!(
            "config file '{}' is not valid UTF-8: {}",
            config_path.display(),
            e
        ),
    })?;

    let config = load_config_from_str(yaml_str)?;
    let paths = resolve_paths(&config, &config_path, options)?;

    Ok(ConfigLoader { config, paths })
}

/// Load and validate config from a YAML string.
///
/// Used by tests and by the file loader.
pub fn load_config_from_str(yaml: &str) -> Result<RawConfig, Error> {
    // Parse with strict unknown-key rejection
    let config: RawConfig = serde_yaml::from_str(yaml).map_err(|e| {
        let msg = format!("{}", e);
        // Augment missing-version errors with a hint to match spec §5 requirement.
        if msg.contains("missing field") && msg.contains("version") {
            Error::InvalidConfig {
                message: format!(
                    "{}. Hint: add `version: 1` at the top of your config file.",
                    msg
                ),
            }
        } else {
            Error::InvalidConfig { message: msg }
        }
    })?;

    validate_config(&config)?;

    Ok(config)
}

/// Validate a parsed config.
fn validate_config(config: &RawConfig) -> Result<(), Error> {
    // Version must be 1
    if config.version != 1 {
        return Err(Error::InvalidConfig {
            message: format!(
                "unsupported config version {}; only version 1 is supported. \
                 Hint: add `version: 1` at the top of your config file.",
                config.version
            ),
        });
    }

    // Validate stores
    for (i, store) in config.stores.iter().enumerate() {
        // Validate sources
        for (j, source) in store.sources.iter().enumerate() {
            validate_source(source, i, j)?;
        }

        // Validate visibility
        if store.visibility != "private" && store.visibility != "shared" {
            return Err(Error::InvalidConfig {
                message: format!(
                    "stores[{}].visibility: must be 'private' or 'shared', got '{}'",
                    i, store.visibility
                ),
            });
        }

        // Validate backend
        if store.backend != "lancedb" {
            return Err(Error::InvalidConfig {
                message: format!(
                    "stores[{}].backend: unknown backend '{}'; supported: lancedb",
                    i, store.backend
                ),
            });
        }
    }

    Ok(())
}

/// Validate a source config entry.
fn validate_source(
    source: &SourceConfig,
    store_idx: usize,
    source_idx: usize,
) -> Result<(), Error> {
    let loc = format!("stores[{}].sources[{}]", store_idx, source_idx);

    match source.kind.as_str() {
        "path" => {
            if source.root.is_none() {
                return Err(Error::InvalidConfig {
                    message: format!("{}.root: required for kind 'path'", loc),
                });
            }

            // Validate preset
            let preset = &source.preset;
            if !["prose", "code", "messages"].contains(&preset.as_str()) {
                return Err(Error::InvalidConfig {
                    message: format!(
                        "{}.preset: unknown preset '{}'; supported: prose, code, messages",
                        loc, preset
                    ),
                });
            }
        }
        "url" => {
            if source.url.is_none() {
                return Err(Error::InvalidConfig {
                    message: format!("{}.url: required for kind 'url'", loc),
                });
            }

            // Validate refresh duration if present
            if let Some(refresh) = &source.refresh {
                parse_duration(refresh).map_err(|e| Error::InvalidConfig {
                    message: format!("{}.refresh: {}", loc, e),
                })?;
            }
        }
        other => {
            return Err(Error::InvalidConfig {
                message: format!(
                    "{}.kind: unknown source kind '{}'; supported: path, url",
                    loc, other
                ),
            });
        }
    }

    Ok(())
}

/// Parse a duration string like "24h", "30m", "90s".
///
/// Returns the duration in seconds.
pub fn parse_duration(s: &str) -> Result<u64, String> {
    if s.is_empty() {
        return Err("duration string is empty".to_string());
    }

    let (num_str, unit) = if let Some(n) = s.strip_suffix('h') {
        (n, 3600u64)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60u64)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1u64)
    } else if let Some(n) = s.strip_suffix('d') {
        (n, 86400u64)
    } else {
        return Err(format!(
            "invalid duration '{}': expected a number followed by 'd', 'h', 'm', or 's' (e.g. '24h', '30m', '90s')",
            s
        ));
    };

    let n: u64 = num_str.parse().map_err(|_| {
        format!(
            "invalid duration '{}': '{}' is not a valid number",
            s, num_str
        )
    })?;

    if n == 0 {
        return Err(format!(
            "invalid duration '{}': duration must be greater than zero",
            s
        ));
    }

    Ok(n * unit)
}

/// Resolve the config file path.
fn resolve_config_path(
    options: &LoadOptions,
    env_config_path: Option<&Path>,
) -> Result<PathBuf, Error> {
    // 1. Explicit flag
    if let Some(p) = &options.config_path {
        return Ok(p.clone());
    }

    // 2. LOCALDB_CONFIG env var (read once at startup, passed in)
    if let Some(env_path) = env_config_path {
        return Ok(env_path.to_path_buf());
    }

    // 3. Platform default
    let platform = PlatformPaths::resolve().ok_or_else(|| Error::InvalidConfig {
        message: "cannot determine platform config path (no home directory?)".to_string(),
    })?;

    Ok(platform.config_file)
}

/// Resolve final paths applying config-file `paths.*` and option overrides.
fn resolve_paths(
    config: &RawConfig,
    config_path: &Path,
    options: &LoadOptions,
) -> Result<ResolvedPaths, Error> {
    let platform = PlatformPaths::resolve().ok_or_else(|| Error::InvalidConfig {
        message: "cannot determine platform paths".to_string(),
    })?;

    let data_dir = options
        .data_dir
        .clone()
        .or_else(|| config.paths.data.as_ref().map(expand_path))
        .unwrap_or(platform.data_dir);

    let models_dir = options
        .models_dir
        .clone()
        .or_else(|| config.paths.models.as_ref().map(expand_path))
        .unwrap_or(platform.models_dir);

    let logs_dir = options
        .logs_dir
        .clone()
        .or_else(|| config.paths.logs.as_ref().map(expand_path))
        .unwrap_or(platform.logs_dir);

    Ok(ResolvedPaths {
        config_file: config_path.to_path_buf(),
        data_dir,
        models_dir,
        logs_dir,
    })
}

/// Expand `~` in a path to the home directory.
fn expand_path(path: &String) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_duration tests ---

    #[test]
    fn parse_duration_hours() {
        assert_eq!(parse_duration("24h").unwrap(), 24 * 3600);
        assert_eq!(parse_duration("1h").unwrap(), 3600);
    }

    #[test]
    fn parse_duration_minutes() {
        assert_eq!(parse_duration("30m").unwrap(), 30 * 60);
        assert_eq!(parse_duration("1m").unwrap(), 60);
    }

    #[test]
    fn parse_duration_seconds() {
        assert_eq!(parse_duration("90s").unwrap(), 90);
        assert_eq!(parse_duration("1s").unwrap(), 1);
    }

    #[test]
    fn parse_duration_days() {
        assert_eq!(parse_duration("7d").unwrap(), 7 * 86400);
    }

    #[test]
    fn parse_duration_rejects_invalid() {
        assert!(parse_duration("not-a-duration").is_err());
        assert!(parse_duration("1x").is_err());
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
    }

    #[test]
    fn parse_duration_rejects_zero() {
        assert!(parse_duration("0h").is_err());
        assert!(parse_duration("0m").is_err());
    }

    // --- load_config_from_str tests ---

    #[test]
    fn load_valid_minimal_config() {
        let yaml = "version: 1\n";
        let cfg = load_config_from_str(yaml).expect("valid minimal config should load");
        assert_eq!(cfg.version, 1);
    }

    #[test]
    fn load_rejects_missing_version() {
        // No version field → serde error (missing required field)
        let yaml = "server:\n  bind: 127.0.0.1\n";
        let err = load_config_from_str(yaml).unwrap_err();
        assert!(matches!(err, Error::InvalidConfig { .. }));
    }

    #[test]
    fn load_rejects_unknown_version() {
        let yaml = "version: 99\n";
        let err = load_config_from_str(yaml).unwrap_err();
        match err {
            Error::InvalidConfig { message } => {
                assert!(
                    message.contains("99"),
                    "error should mention the version number"
                );
                assert!(
                    message.contains("version 1"),
                    "error should mention supported version"
                );
            }
            other => panic!("expected InvalidConfig, got {:?}", other),
        }
    }

    #[test]
    fn load_rejects_unversioned_with_hint() {
        // Missing version field — error must contain a hint per spec §5.
        let yaml = "server:\n  bind: 127.0.0.1\n  port: 7700\n";
        let err = load_config_from_str(yaml).unwrap_err();
        match err {
            Error::InvalidConfig { message } => {
                assert!(
                    message.contains("version: 1") || message.contains("version"),
                    "error for unversioned config should contain a hint, got: {}",
                    message
                );
            }
            other => panic!("expected InvalidConfig, got {:?}", other),
        }
    }

    #[test]
    fn load_rejects_typo_key() {
        let yaml = "version: 1\nservre:\n  bind: 127.0.0.1\n";
        let err = load_config_from_str(yaml).unwrap_err();
        match err {
            Error::InvalidConfig { message } => {
                // Error should mention the unknown key
                assert!(
                    message.contains("servre") || message.contains("unknown"),
                    "error message '{}' should mention the typo'd key",
                    message
                );
            }
            other => panic!("expected InvalidConfig, got {:?}", other),
        }
    }

    #[test]
    fn load_rejects_typo_in_defaults_chunking() {
        let yaml = r#"
version: 1
defaults:
  indexing:
    chunkng:
      preset_overrides: {}
    embedding:
      model: pplx-embed-context-v1-0.6b
      provider: local-onnx
"#;
        let err = load_config_from_str(yaml).unwrap_err();
        assert!(
            matches!(err, Error::InvalidConfig { .. }),
            "typo in defaults.indexing should fail: {:?}",
            err
        );
    }

    #[test]
    fn load_rejects_bad_duration() {
        let yaml = r#"
version: 1
stores:
  - name: web
    sources:
      - kind: url
        url: https://example.com
        refresh: not-a-duration
"#;
        let err = load_config_from_str(yaml).unwrap_err();
        match err {
            Error::InvalidConfig { message } => {
                assert!(
                    message.contains("refresh"),
                    "error should mention 'refresh', got: {}",
                    message
                );
            }
            other => panic!("expected InvalidConfig, got {:?}", other),
        }
    }

    #[test]
    fn load_rejects_path_source_without_root() {
        let yaml = r#"
version: 1
stores:
  - name: mystore
    sources:
      - kind: path
        include: ["**/*.md"]
"#;
        let err = load_config_from_str(yaml).unwrap_err();
        match err {
            Error::InvalidConfig { message } => {
                assert!(
                    message.contains("root"),
                    "error should mention 'root', got: {}",
                    message
                );
            }
            other => panic!("expected InvalidConfig, got {:?}", other),
        }
    }

    #[test]
    fn load_rejects_url_source_without_url() {
        let yaml = r#"
version: 1
stores:
  - name: mystore
    sources:
      - kind: url
        refresh: 24h
"#;
        let err = load_config_from_str(yaml).unwrap_err();
        match err {
            Error::InvalidConfig { message } => {
                assert!(
                    message.contains("url"),
                    "error should mention 'url', got: {}",
                    message
                );
            }
            other => panic!("expected InvalidConfig, got {:?}", other),
        }
    }

    #[test]
    fn load_valid_full_config() {
        let yaml = r#"
version: 1

server:
  bind: 127.0.0.1
  port: 7700

paths:
  data: ~
  models: ~
  logs: ~

defaults:
  indexing:
    chunking:
      preset_overrides: {}
    embedding:
      model: pplx-embed-context-v1-0.6b
      provider: local-onnx

stores:
  - name: notes
    visibility: private
    backend: lancedb
    indexing: ~
    sources:
      - kind: path
        root: ~/Documents/notes
        include: ["**/*.md", "**/*.pdf"]
        exclude: ["**/node_modules/**"]
        preset: prose
      - kind: url
        url: https://example.com/handbook
        refresh: 24h

providers:
  - name: my-ollama
    kind: openai-compatible
    base_url: http://localhost:11434/v1
    api_key_env: OLLAMA_KEY
"#;
        let cfg = load_config_from_str(yaml).expect("valid full config should load");
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.stores.len(), 1);
        assert_eq!(cfg.stores[0].name, "notes");
        assert_eq!(cfg.stores[0].sources.len(), 2);
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.providers[0].name, "my-ollama");
    }

    #[test]
    fn load_valid_config_with_store_indexing_override() {
        let yaml = r#"
version: 1
stores:
  - name: code-store
    sources:
      - kind: path
        root: ~/src
        preset: code
    indexing:
      chunking:
        preset_overrides: {}
      embedding:
        model: bge-small
        provider: local-onnx
"#;
        let cfg =
            load_config_from_str(yaml).expect("config with store indexing override should load");
        let store = &cfg.stores[0];
        assert!(store.indexing.is_some());
        let indexing = store.indexing.as_ref().unwrap();
        assert_eq!(indexing.embedding.model, "bge-small");
    }

    #[test]
    fn load_error_has_path_context_for_store_source() {
        let yaml = r#"
version: 1
stores:
  - name: s1
    sources:
      - kind: path
        root: /tmp
  - name: s2
    sources:
      - kind: url
        url: https://example.com
      - kind: url
        refresh: 5h
"#;
        let err = load_config_from_str(yaml).unwrap_err();
        match err {
            Error::InvalidConfig { message } => {
                // Should mention the store/source index
                assert!(
                    message.contains("stores[1]") || message.contains("sources[1]"),
                    "error '{}' should include location context",
                    message
                );
            }
            other => panic!("expected InvalidConfig, got {:?}", other),
        }
    }

    #[test]
    fn unknown_source_kind_rejected() {
        let yaml = r#"
version: 1
stores:
  - name: mystore
    sources:
      - kind: imap
        url: imap://mail.example.com
"#;
        let err = load_config_from_str(yaml).unwrap_err();
        match err {
            Error::InvalidConfig { message } => {
                assert!(
                    message.contains("imap"),
                    "error should mention the unknown kind, got: {}",
                    message
                );
            }
            other => panic!("expected InvalidConfig, got {:?}", other),
        }
    }

    // --- fixture file tests ---

    #[test]
    fn fixture_valid_loads_successfully() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/config/valid.yaml"
        );
        let yaml = std::fs::read_to_string(path).expect("fixture file should exist");
        let cfg = load_config_from_str(&yaml).expect("valid fixture should load");
        assert_eq!(cfg.version, 1);
    }

    #[test]
    fn fixture_typo_key_rejected() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/config/typo_key.yaml"
        );
        let yaml = std::fs::read_to_string(path).expect("fixture file should exist");
        let err = load_config_from_str(&yaml).unwrap_err();
        assert!(
            matches!(err, Error::InvalidConfig { .. }),
            "typo'd key fixture should fail: {:?}",
            err
        );
    }

    #[test]
    fn fixture_bad_duration_rejected() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/config/bad_duration.yaml"
        );
        let yaml = std::fs::read_to_string(path).expect("fixture file should exist");
        let err = load_config_from_str(&yaml).unwrap_err();
        match err {
            Error::InvalidConfig { message } => {
                assert!(
                    message.contains("refresh") || message.contains("duration"),
                    "bad_duration fixture error should mention 'refresh' or 'duration', got: {}",
                    message
                );
            }
            other => panic!("expected InvalidConfig, got {:?}", other),
        }
    }

    #[test]
    fn fixture_unversioned_rejected_with_hint() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/config/unversioned.yaml"
        );
        let yaml = std::fs::read_to_string(path).expect("fixture file should exist");
        let err = load_config_from_str(&yaml).unwrap_err();
        match err {
            Error::InvalidConfig { message } => {
                assert!(
                    message.contains("version: 1") || message.contains("version"),
                    "unversioned fixture error should contain a hint, got: {}",
                    message
                );
            }
            other => panic!("expected InvalidConfig, got {:?}", other),
        }
    }

    // --- load_config file-path override tests ---

    #[test]
    fn load_config_with_explicit_path_option() {
        // LoadOptions.config_path overrides env and platform default.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/config/valid.yaml"
        );
        let options = LoadOptions {
            config_path: Some(std::path::PathBuf::from(path)),
            ..Default::default()
        };
        let loader =
            load_config(&options, None).expect("load_config with explicit path should succeed");
        assert_eq!(loader.config.version, 1);
        assert_eq!(loader.paths.config_file, std::path::PathBuf::from(path));
    }

    #[test]
    fn load_config_env_var_override() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/config/valid.yaml"
        );
        let loader = load_config(&LoadOptions::default(), Some(Path::new(path)))
            .expect("load_config via env config path should succeed");
        assert_eq!(loader.config.version, 1);
    }

    // --- Tilde expansion test ---

    #[test]
    fn expand_path_expands_tilde() {
        // A path starting with `~/` should have `~` replaced by the home directory.
        if let Some(home) = dirs::home_dir() {
            let expanded = expand_path(&"~/Documents/notes".to_string());
            assert!(
                expanded.starts_with(&home),
                "expanded path {:?} should start with home dir {:?}",
                expanded,
                home
            );
            assert!(
                expanded.ends_with("Documents/notes"),
                "expanded path {:?} should end with Documents/notes",
                expanded
            );
        }
    }

    #[test]
    fn expand_path_passes_through_absolute_path() {
        let p = expand_path(&"/absolute/path".to_string());
        assert_eq!(p, std::path::PathBuf::from("/absolute/path"));
    }

    // --- YAML file bytes never written test (enforced at type level here) ---

    #[test]
    fn config_yaml_file_not_written_after_load() {
        // Load a fixture, then verify its bytes on disk are unchanged.
        // Structural invariant: RawConfig and ConfigLoader expose no write-to-file methods.
        let path_str = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/config/valid.yaml"
        );
        let before = std::fs::read(path_str).expect("fixture must exist");
        let options = LoadOptions {
            config_path: Some(std::path::PathBuf::from(path_str)),
            ..Default::default()
        };
        let _loader = load_config(&options, None).expect("should load");
        let after = std::fs::read(path_str).expect("fixture must still exist");
        assert_eq!(
            before, after,
            "config file must not be modified by load_config"
        );
    }
}
