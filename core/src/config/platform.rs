//! Platform-specific path resolution.
//!
//! Resolves default paths for config, data, models, and logs
//! using the `directories` crate (XDG on Linux, Apple conventions on macOS).
//!
//! See specs/03-config.md §4.

use std::path::PathBuf;

/// Resolved platform paths for localdb.
///
/// All paths are absolute. Overrides from `paths.*` in config or env vars
/// take precedence over these defaults.
#[derive(Debug, Clone)]
pub struct PlatformPaths {
    /// Config file path.
    /// macOS: `~/Library/Application Support/localdb/config.yaml`
    /// Linux: `$XDG_CONFIG_HOME/localdb/config.yaml`
    pub config_file: PathBuf,

    /// Data directory (indexes, runtime-state DB, lock, socket).
    /// macOS: `~/Library/Application Support/localdb/data/`
    /// Linux: `$XDG_DATA_HOME/localdb/`
    pub data_dir: PathBuf,

    /// Model cache directory.
    /// macOS: `~/Library/Caches/localdb/models/`
    /// Linux: `$XDG_CACHE_HOME/localdb/models/`
    pub models_dir: PathBuf,

    /// Log directory.
    /// macOS: `~/Library/Logs/localdb/`
    /// Linux: `$XDG_STATE_HOME/localdb/logs/`
    pub logs_dir: PathBuf,
}

impl PlatformPaths {
    /// Resolve platform default paths.
    ///
    /// Returns `None` if the platform cannot determine home directory
    /// (unusual; only happens in no-home-dir environments).
    pub fn resolve() -> Option<Self> {
        Self::resolve_with_qualifier("", "", "localdb")
    }

    /// Resolve platform paths with explicit qualifier/organization/application.
    ///
    /// Exposed for testing — allows injecting a custom application name
    /// so tests don't pollute real user data directories.
    pub fn resolve_with_qualifier(
        qualifier: &str,
        organization: &str,
        application: &str,
    ) -> Option<Self> {
        use directories::ProjectDirs;

        let dirs = ProjectDirs::from(qualifier, organization, application)?;

        let config_file = dirs.config_dir().join("config.yaml");

        // macOS: Application Support/<app>/data/  Linux: $XDG_DATA_HOME/<app>/
        let data_dir = {
            #[cfg(target_os = "macos")]
            {
                dirs.data_dir().join("data")
            }
            #[cfg(not(target_os = "macos"))]
            {
                dirs.data_dir().to_path_buf()
            }
        };

        // macOS: ~/Library/Caches/<app>/models/  Linux: $XDG_CACHE_HOME/<app>/models/
        let models_dir = dirs.cache_dir().join("models");

        // macOS: ~/Library/Logs/<app>/  Linux: $XDG_STATE_HOME/<app>/logs/
        let logs_dir = {
            #[cfg(target_os = "macos")]
            {
                // On macOS, ProjectDirs doesn't expose Logs directly; build manually
                let home = dirs::home_dir()?;
                home.join("Library").join("Logs").join(application)
            }
            #[cfg(not(target_os = "macos"))]
            {
                dirs.state_dir()
                    .map(|d| d.join("logs"))
                    .unwrap_or_else(|| dirs.data_dir().join("logs"))
            }
        };

        Some(Self {
            config_file,
            data_dir,
            models_dir,
            logs_dir,
        })
    }

    /// Path to the unix socket (`<data_dir>/daemon.sock`).
    pub fn socket_path(&self) -> PathBuf {
        self.data_dir.join("daemon.sock")
    }

    /// Path to the write lock file (`<data_dir>/.write.lock`).
    pub fn write_lock_path(&self) -> PathBuf {
        self.data_dir.join(".write.lock")
    }

    /// Path to the runtime-state DB (`<data_dir>/runtime-state.db`).
    pub fn runtime_state_db_path(&self) -> PathBuf {
        self.data_dir.join("runtime-state.db")
    }

    /// Path to the unified single-file DB (`<data_dir>/localdb.db`).
    pub fn unified_db_path(&self) -> PathBuf {
        self.data_dir.join("localdb.db")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_paths_resolve_succeeds() {
        // Basic sanity — should resolve on any CI runner
        let paths = PlatformPaths::resolve();
        assert!(paths.is_some(), "platform paths should resolve");
    }

    #[test]
    fn platform_paths_are_absolute() {
        let paths = PlatformPaths::resolve().expect("should resolve");
        assert!(
            paths.config_file.is_absolute(),
            "config_file must be absolute"
        );
        assert!(paths.data_dir.is_absolute(), "data_dir must be absolute");
        assert!(
            paths.models_dir.is_absolute(),
            "models_dir must be absolute"
        );
        assert!(paths.logs_dir.is_absolute(), "logs_dir must be absolute");
    }

    #[test]
    fn socket_path_is_inside_data_dir() {
        let paths = PlatformPaths::resolve().expect("should resolve");
        let sock = paths.socket_path();
        assert!(sock.starts_with(&paths.data_dir));
        assert_eq!(sock.file_name().unwrap(), "daemon.sock");
    }

    #[test]
    fn write_lock_path_is_inside_data_dir() {
        let paths = PlatformPaths::resolve().expect("should resolve");
        let lock = paths.write_lock_path();
        assert!(lock.starts_with(&paths.data_dir));
        assert_eq!(lock.file_name().unwrap(), ".write.lock");
    }

    #[test]
    fn runtime_state_db_path_is_inside_data_dir() {
        let paths = PlatformPaths::resolve().expect("should resolve");
        let db = paths.runtime_state_db_path();
        assert!(db.starts_with(&paths.data_dir));
        assert_eq!(db.file_name().unwrap(), "runtime-state.db");
    }

    #[test]
    fn config_file_ends_with_config_yaml() {
        let paths = PlatformPaths::resolve().expect("should resolve");
        assert_eq!(paths.config_file.file_name().unwrap(), "config.yaml");
    }

    #[test]
    fn models_dir_ends_with_models() {
        let paths = PlatformPaths::resolve().expect("should resolve");
        let last = paths.models_dir.file_name().unwrap();
        assert_eq!(last, "models");
    }
}
