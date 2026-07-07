//! Read-only lookup of cached daemon credentials (specs/03-config.md §6).
//!
//! `credentials.json` lives next to `config.yaml` and caches locally-issued
//! API keys/tokens, keyed by the daemon's base URL so a machine talking to
//! multiple daemons keeps a separate credential per one. T3 ships only this
//! reader — the writer (`localdb login`) lands in T4. Canonical shape:
//!
//! ```json
//! {
//!   "version": 1,
//!   "credentials": {
//!     "http://127.0.0.1:7700": { "secret": "ldb_..." }
//!   }
//! }
//! ```
//!
//! The `LOCALDB_API_KEY` environment variable, when set (read once at
//! startup into `CliContext::api_key`), overrides the cached credential for
//! that invocation.

use std::path::{Path, PathBuf};

/// The `credentials.json` path for a given config file path (sibling file).
pub(crate) fn credentials_path(config_file: &Path) -> PathBuf {
    config_file.with_file_name("credentials.json")
}

/// Normalize a daemon base URL for use as a credentials key: trailing
/// slashes are insignificant (`http://x:7700/` ≡ `http://x:7700`).
fn normalize_base_url(base_url: &str) -> &str {
    base_url.trim_end_matches('/')
}

/// Look up the cached secret for `base_url` in the credentials file, if the
/// file exists, parses, and has an entry. Any failure (missing file,
/// malformed JSON, absent key) is a silent `None` — a missing credential
/// simply means the request goes out without a bearer token, and the daemon
/// answers 401 with a clear message if it needed one.
pub(crate) fn lookup_secret(credentials_file: &Path, base_url: &str) -> Option<String> {
    let contents = std::fs::read_to_string(credentials_file).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&contents).ok()?;
    parsed
        .get("credentials")?
        .get(normalize_base_url(base_url))?
        .get("secret")?
        .as_str()
        .map(|s| s.to_string())
}

/// Resolve the bearer secret for a daemon request:
/// 1. `api_key` (from `LOCALDB_API_KEY`, read once at startup) if set;
/// 2. else the `credentials.json` entry next to `config_file` keyed by
///    `base_url`;
/// 3. else `None` (send no Authorization header).
pub(crate) fn resolve_bearer(
    api_key: Option<&str>,
    config_file: Option<&Path>,
    base_url: &str,
) -> Option<String> {
    if let Some(key) = api_key {
        if !key.is_empty() {
            return Some(key.to_string());
        }
    }
    let config_file = config_file?;
    lookup_secret(&credentials_path(config_file), base_url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_credentials(dir: &TempDir, body: &str) -> PathBuf {
        let path = dir.path().join("credentials.json");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn credentials_path_is_sibling_of_config() {
        assert_eq!(
            credentials_path(Path::new("/etc/localdb/config.yaml")),
            PathBuf::from("/etc/localdb/credentials.json")
        );
    }

    #[test]
    fn lookup_finds_secret_by_base_url() {
        let dir = TempDir::new().unwrap();
        let path = write_credentials(
            &dir,
            r#"{"version":1,"credentials":{"http://127.0.0.1:7700":{"secret":"ldb_cached"}}}"#,
        );
        assert_eq!(
            lookup_secret(&path, "http://127.0.0.1:7700").as_deref(),
            Some("ldb_cached")
        );
    }

    #[test]
    fn lookup_normalizes_trailing_slash() {
        let dir = TempDir::new().unwrap();
        let path = write_credentials(
            &dir,
            r#"{"version":1,"credentials":{"http://127.0.0.1:7700":{"secret":"ldb_cached"}}}"#,
        );
        assert_eq!(
            lookup_secret(&path, "http://127.0.0.1:7700/").as_deref(),
            Some("ldb_cached")
        );
    }

    #[test]
    fn lookup_returns_none_for_unknown_url() {
        let dir = TempDir::new().unwrap();
        let path = write_credentials(
            &dir,
            r#"{"version":1,"credentials":{"http://127.0.0.1:7700":{"secret":"ldb_cached"}}}"#,
        );
        assert_eq!(lookup_secret(&path, "http://10.0.0.5:7700"), None);
    }

    #[test]
    fn lookup_returns_none_for_missing_file() {
        let dir = TempDir::new().unwrap();
        assert_eq!(
            lookup_secret(&dir.path().join("credentials.json"), "http://x"),
            None
        );
    }

    #[test]
    fn lookup_returns_none_for_malformed_json() {
        let dir = TempDir::new().unwrap();
        let path = write_credentials(&dir, "not json at all");
        assert_eq!(lookup_secret(&path, "http://127.0.0.1:7700"), None);
    }

    #[test]
    fn resolve_bearer_prefers_env_api_key() {
        let dir = TempDir::new().unwrap();
        write_credentials(
            &dir,
            r#"{"version":1,"credentials":{"http://127.0.0.1:7700":{"secret":"ldb_cached"}}}"#,
        );
        let config_file = dir.path().join("config.yaml");
        let resolved = resolve_bearer(
            Some("ldb_from_env"),
            Some(&config_file),
            "http://127.0.0.1:7700",
        );
        assert_eq!(resolved.as_deref(), Some("ldb_from_env"));
    }

    #[test]
    fn resolve_bearer_falls_back_to_credentials_file() {
        let dir = TempDir::new().unwrap();
        write_credentials(
            &dir,
            r#"{"version":1,"credentials":{"http://127.0.0.1:7700":{"secret":"ldb_cached"}}}"#,
        );
        let config_file = dir.path().join("config.yaml");
        let resolved = resolve_bearer(None, Some(&config_file), "http://127.0.0.1:7700");
        assert_eq!(resolved.as_deref(), Some("ldb_cached"));
    }

    #[test]
    fn resolve_bearer_empty_env_key_is_ignored() {
        let dir = TempDir::new().unwrap();
        write_credentials(
            &dir,
            r#"{"version":1,"credentials":{"http://127.0.0.1:7700":{"secret":"ldb_cached"}}}"#,
        );
        let config_file = dir.path().join("config.yaml");
        let resolved = resolve_bearer(Some(""), Some(&config_file), "http://127.0.0.1:7700");
        assert_eq!(resolved.as_deref(), Some("ldb_cached"));
    }

    #[test]
    fn resolve_bearer_none_when_nothing_available() {
        let dir = TempDir::new().unwrap();
        let config_file = dir.path().join("config.yaml");
        assert_eq!(
            resolve_bearer(None, Some(&config_file), "http://127.0.0.1:7700"),
            None
        );
        assert_eq!(resolve_bearer(None, None, "http://127.0.0.1:7700"), None);
    }
}
