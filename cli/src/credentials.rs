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

/// A stable sibling lock survives replacement of credentials.json. Hold it across
/// read/refresh/write, and across logout, so all processes observe one mutation order.
/// Independent file handles also serialize tasks within one process.
pub(crate) struct CredentialLock {
    path: PathBuf,
    _file: std::fs::File,
}

impl CredentialLock {
    pub(crate) async fn acquire(path: &Path) -> std::io::Result<Self> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path.with_file_name("credentials.lock"))?;
        loop {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => {
                    return Ok(Self {
                        path: path.to_owned(),
                        _file: file,
                    })
                }
                Err(error)
                    if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub(crate) fn lookup(&self, base_url: &str) -> Option<CredentialEntry> {
        lookup_entry(&self.path, base_url)
    }
    pub(crate) fn write(&self, base_url: &str, entry: CredentialEntry) -> std::io::Result<()> {
        write_entry(&self.path, base_url, entry)
    }
    pub(crate) fn remove(&self, base_url: &str) -> std::io::Result<bool> {
        remove_entry(&self.path, base_url)
    }
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
mod tests;
