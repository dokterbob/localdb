//! Lookup and atomic writer for cached daemon credentials
//! (specs/03-config.md §6).
//!
//! `credentials.json` lives next to `config.yaml` and caches locally-issued
//! API keys/tokens, keyed by the daemon's base URL so a machine talking to
//! multiple daemons keeps a separate credential per one. T3 shipped the
//! read-only lookup for the legacy `{"secret": "ldb_..."}` API-key shape;
//! T4 (`localdb login`/`logout`) adds an atomic writer plus a richer entry
//! shape carrying an access/refresh token pair:
//!
//! ```json
//! {
//!   "version": 1,
//!   "credentials": {
//!     "http://127.0.0.1:7700": { "secret": "ldb_..." },
//!     "http://127.0.0.1:7701": {
//!       "access_token": "ldb_...",
//!       "refresh_token": "ldb_...",
//!       "access_expires_at": "2026-07-07T13:00:00Z"
//!     }
//!   }
//! }
//! ```
//!
//! Both shapes coexist in the same file — an API key entry (`secret`, from
//! `localdb key create` pasted manually, or a pre-T4 file) and a login-token
//! entry (`access_token`/`refresh_token`/`access_expires_at`, from
//! `localdb login`) are both valid per base-URL entries; `lookup_secret`
//! prefers `access_token` when both are present. The `LOCALDB_API_KEY`
//! environment variable, when set (read once at startup into
//! `CliContext::api_key`), overrides the cached credential for that
//! invocation, taking priority over either shape.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A single base-URL's cached credential. Every field is optional so the
/// same struct represents both the legacy API-key shape (`secret` only)
/// and the T4 login-token shape (`access_token`/`refresh_token`/
/// `access_expires_at`) — and tolerates whichever fields a future version
/// adds, since unknown fields are simply absent here rather than rejected.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CredentialEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_expires_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CredentialsFile {
    version: u32,
    credentials: BTreeMap<String, CredentialEntry>,
}

impl Default for CredentialsFile {
    fn default() -> Self {
        Self {
            version: 1,
            credentials: BTreeMap::new(),
        }
    }
}

/// The `credentials.json` path for a given config file path (sibling file).
pub(crate) fn credentials_path(config_file: &Path) -> PathBuf {
    config_file.with_file_name("credentials.json")
}

/// Normalize a daemon base URL for use as a credentials key: trailing
/// slashes are insignificant (`http://x:7700/` ≡ `http://x:7700`).
fn normalize_base_url(base_url: &str) -> &str {
    base_url.trim_end_matches('/')
}

/// Load the credentials file, or a fresh empty one if it's missing,
/// unreadable, or malformed — a broken/absent file simply means no cached
/// credentials exist yet, never a hard error.
fn load_file(credentials_file: &Path) -> CredentialsFile {
    std::fs::read_to_string(credentials_file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Look up the cached entry for `base_url`, if the file exists, parses, and
/// has one.
pub(crate) fn lookup_entry(credentials_file: &Path, base_url: &str) -> Option<CredentialEntry> {
    let file = load_file(credentials_file);
    file.credentials.get(normalize_base_url(base_url)).cloned()
}

/// Look up the cached secret for `base_url`: an `access_token` (from
/// `localdb login`) is preferred when present, falling back to the legacy
/// `secret` (API key) field. Any failure (missing file, malformed JSON,
/// absent key) is a silent `None` — a missing credential simply means the
/// request goes out without a bearer token, and the daemon answers 401 with
/// a clear message if it needed one.
pub(crate) fn lookup_secret(credentials_file: &Path, base_url: &str) -> Option<String> {
    let entry = lookup_entry(credentials_file, base_url)?;
    entry.access_token.or(entry.secret)
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

/// Atomically insert/replace the entry for `base_url`: read-modify-write via
/// a temp file in the same directory followed by a rename (atomic on the
/// same filesystem), with `0600` permissions set on the temp file before
/// the rename so the secret is never briefly world/group-readable.
pub(crate) fn write_entry(
    credentials_file: &Path,
    base_url: &str,
    entry: CredentialEntry,
) -> std::io::Result<()> {
    let mut file = load_file(credentials_file);
    file.credentials
        .insert(normalize_base_url(base_url).to_string(), entry);
    write_file(credentials_file, &file)
}

/// Remove the entry for `base_url`, if any. Returns `true` if an entry was
/// removed. Same atomic write as [`write_entry`].
pub(crate) fn remove_entry(credentials_file: &Path, base_url: &str) -> std::io::Result<bool> {
    let mut file = load_file(credentials_file);
    let removed = file
        .credentials
        .remove(normalize_base_url(base_url))
        .is_some();
    write_file(credentials_file, &file)?;
    Ok(removed)
}

fn write_file(credentials_file: &Path, file: &CredentialsFile) -> std::io::Result<()> {
    let dir = credentials_file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(file).unwrap_or_default();

    let tmp_name = format!(".credentials.json.tmp-{}", std::process::id());
    let tmp_path = dir.join(tmp_name);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
    }

    std::fs::rename(&tmp_path, credentials_file)?;
    Ok(())
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

    // -----------------------------------------------------------------
    // T4: writer, login-token shape, atomicity, permissions
    // -----------------------------------------------------------------

    #[test]
    fn write_entry_then_lookup_round_trips_access_token() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("credentials.json");
        let entry = CredentialEntry {
            secret: None,
            access_token: Some("ldb_access".to_string()),
            refresh_token: Some("ldb_refresh".to_string()),
            access_expires_at: Some("2026-07-07T13:00:00Z".to_string()),
        };
        write_entry(&path, "http://127.0.0.1:7700", entry.clone()).unwrap();

        let found = lookup_entry(&path, "http://127.0.0.1:7700").unwrap();
        assert_eq!(found, entry);
        assert_eq!(
            lookup_secret(&path, "http://127.0.0.1:7700").as_deref(),
            Some("ldb_access"),
            "access_token is preferred over secret when both could apply"
        );
    }

    #[test]
    fn write_entry_preserves_other_base_urls() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("credentials.json");
        write_entry(
            &path,
            "http://127.0.0.1:7700",
            CredentialEntry {
                secret: Some("ldb_one".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        write_entry(
            &path,
            "http://127.0.0.1:7701",
            CredentialEntry {
                secret: Some("ldb_two".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            lookup_secret(&path, "http://127.0.0.1:7700").as_deref(),
            Some("ldb_one")
        );
        assert_eq!(
            lookup_secret(&path, "http://127.0.0.1:7701").as_deref(),
            Some("ldb_two")
        );
    }

    #[test]
    fn write_entry_overwrites_same_base_url() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("credentials.json");
        write_entry(
            &path,
            "http://127.0.0.1:7700",
            CredentialEntry {
                secret: Some("ldb_old".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        write_entry(
            &path,
            "http://127.0.0.1:7700",
            CredentialEntry {
                access_token: Some("ldb_new".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            lookup_secret(&path, "http://127.0.0.1:7700").as_deref(),
            Some("ldb_new")
        );
    }

    #[test]
    fn write_entry_sets_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("credentials.json");
        write_entry(
            &path,
            "http://127.0.0.1:7700",
            CredentialEntry {
                secret: Some("ldb_secret".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "credentials.json must be 0600, got {mode:o}");
    }

    #[test]
    fn write_entry_leaves_no_leftover_tmp_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("credentials.json");
        write_entry(
            &path,
            "http://127.0.0.1:7700",
            CredentialEntry {
                secret: Some("ldb_secret".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(
            leftover.is_empty(),
            "no temp file should remain after an atomic write: {leftover:?}"
        );
        assert!(path.exists(), "the final credentials.json must exist");
    }

    #[test]
    fn remove_entry_removes_only_the_named_base_url() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("credentials.json");
        write_entry(
            &path,
            "http://127.0.0.1:7700",
            CredentialEntry {
                secret: Some("ldb_one".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        write_entry(
            &path,
            "http://127.0.0.1:7701",
            CredentialEntry {
                secret: Some("ldb_two".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        let removed = remove_entry(&path, "http://127.0.0.1:7700").unwrap();
        assert!(removed);
        assert!(lookup_entry(&path, "http://127.0.0.1:7700").is_none());
        assert!(lookup_entry(&path, "http://127.0.0.1:7701").is_some());
    }

    #[test]
    fn remove_entry_missing_returns_false() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("credentials.json");
        assert!(!remove_entry(&path, "http://127.0.0.1:7700").unwrap());
    }

    #[test]
    fn legacy_secret_only_file_still_reads_via_lookup_entry() {
        // Backward compatibility: an entry with only `secret` (pre-T4 shape,
        // or an API key a human pasted in by hand) must still round-trip
        // through the richer `CredentialEntry` struct.
        let dir = TempDir::new().unwrap();
        let path = write_credentials(
            &dir,
            r#"{"version":1,"credentials":{"http://127.0.0.1:7700":{"secret":"ldb_legacy"}}}"#,
        );
        let entry = lookup_entry(&path, "http://127.0.0.1:7700").unwrap();
        assert_eq!(entry.secret.as_deref(), Some("ldb_legacy"));
        assert!(entry.access_token.is_none());
    }
}
