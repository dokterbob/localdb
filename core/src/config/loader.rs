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
    config::{platform::PlatformPaths, schema::RawConfig},
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

    /// Path of the daemon discovery URL file.
    ///
    /// The running daemon records its client-reachable base URL here so CLI/MCP
    /// discovery honors the configured bind address and port instead of assuming
    /// `http://127.0.0.1:7700`.
    pub fn url_path(&self) -> PathBuf {
        self.data_dir.join("daemon.url")
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("localdb.db")
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

pub fn refuse_legacy_layout(data_dir: &Path) -> Result<(), Error> {
    let runtime_db = data_dir.join("runtime-state.db");
    let stores_dir = data_dir.join("stores");
    if runtime_db.exists() || stores_dir.exists() {
        return Err(Error::InvalidConfig {
            message: format!(
                "data dir '{}' contains a legacy layout from before v0.1.0 ({}, {}). \
                 There is no migration path; remove the legacy files and re-add stores with \
                 `localdb store add` and `localdb source add`.",
                data_dir.display(),
                runtime_db.display(),
                stores_dir.display()
            ),
        });
    }
    Ok(())
}

/// Load and validate config from a YAML string.
///
/// Used by tests and by the file loader.
pub fn load_config_from_str(yaml: &str) -> Result<RawConfig, Error> {
    // Parse with strict unknown-key rejection
    let mut config: RawConfig = serde_yaml::from_str(yaml).map_err(|e| {
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

    if let Some(public_url) = &mut config.server.public_url {
        let invalid = || {
            Error::InvalidConfig { message: "server.public_url must be an absolute HTTP(S) URL without credentials, query, or fragment".into() }
        };
        let parsed = url::Url::parse(public_url).map_err(|_| invalid())?;
        let authority = public_url.split_once("://").map(|(_, rest)| rest);
        if authority.is_none_or(|rest| rest.is_empty() || rest.starts_with('/'))
            || public_url.contains('\\')
            || !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || public_url
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(invalid());
        }
        *public_url = parsed.as_str().trim_end_matches('/').to_string();
    }
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

    if config.server.job_workers < 1 {
        return Err(Error::InvalidConfig {
            message: "server.job_workers must be greater than zero".to_string(),
        });
    }

    if config.http.rate_limit.requests_per_second < 1 {
        return Err(Error::InvalidConfig {
            message: "http.rate_limit.requests_per_second must be greater than zero".to_string(),
        });
    }

    if config.http.rate_limit.burst < 1 {
        return Err(Error::InvalidConfig {
            message: "http.rate_limit.burst must be greater than zero".to_string(),
        });
    }

    // Rejected here rather than at first use: `user_agent` is handed to
    // `reqwest::ClientBuilder::user_agent`, and a value that is not a legal
    // header (a newline, a control character) makes *every* `build()` fail —
    // including the client an index job constructs before it knows whether it
    // will fetch anything, so a purely path-based job dies too, with an opaque
    // "failed to build HTTP client" instead of a config error naming the key.
    //
    // `HeaderValue::from_str` rather than a hand-rolled ASCII predicate: it is
    // the exact rule reqwest will apply, so the two cannot drift. (It accepts
    // obs-text, 0x80-0xFF, which a naive `is_ascii_graphic` check would
    // wrongly reject.)
    if let Some(user_agent) = &config.http.user_agent {
        if http::HeaderValue::from_str(user_agent).is_err() {
            return Err(Error::InvalidConfig {
                message: format!("http.user_agent {user_agent:?} is not a valid HTTP header value"),
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
pub fn resolve_config_path(
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
mod tests;
