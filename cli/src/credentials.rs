//! Lookup and atomic writer for cached daemon credentials
//! (specs/03-config.md §6).
//!
//! `credentials.json` lives next to `config.yaml` and caches locally-issued
//! API keys/tokens, keyed by the daemon's base URL so a machine talking to
//! multiple daemons keeps a separate credential per one. Two entry shapes
//! coexist: the legacy `{"secret": "ldb_..."}` API-key shape (read-only
//! lookup), and a richer shape written by `localdb login`/`logout` through
//! an atomic writer, carrying an access/refresh token pair:
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
//! `localdb key create` pasted manually, or an older file written before
//! login-token support) and a login-token
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
/// and the login-token shape (`access_token`/`refresh_token`/
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

impl CredentialsFile {
    /// Canonicalize every stored key by trimming a trailing `/`, so a file
    /// that holds a legacy or hand-edited slash-suffixed key behaves exactly
    /// like one that doesn't: every reader and writer goes through
    /// [`load_file`], so canonicalizing here is the single place that
    /// decides it. When two spellings of the same key collide, the entry
    /// stored under the already-canonical key wins, regardless of which one
    /// was read first.
    fn canonicalized(self) -> Self {
        let mut credentials: BTreeMap<String, CredentialEntry> = BTreeMap::new();
        for (key, entry) in self.credentials {
            let is_canonical = normalize_base_url(&key) == key;
            let canonical_key = normalize_base_url(&key).to_string();
            if is_canonical || !credentials.contains_key(&canonical_key) {
                credentials.insert(canonical_key, entry);
            }
        }
        Self {
            version: self.version,
            credentials,
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

/// Load the credentials file strictly, for a mutation: a missing file is
/// empty credentials, but any other read or parse failure is an error
/// naming the file. A mutation must never treat "unreadable" or
/// "malformed" the same as "absent" — doing so would let `write_entry`/
/// `remove_entry` replace a file they couldn't actually parse, destroying
/// every other daemon's cached credential in it.
fn load_file_for_write(credentials_file: &Path) -> std::io::Result<CredentialsFile> {
    let contents = match std::fs::read_to_string(credentials_file) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CredentialsFile::default());
        }
        Err(error) => {
            return Err(std::io::Error::new(
                error.kind(),
                format!("cannot read {}: {error}", credentials_file.display()),
            ));
        }
    };
    let file: CredentialsFile = serde_json::from_str(&contents).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            describe_parse_error(credentials_file, &error),
        )
    })?;
    Ok(file.canonicalized())
}

/// Describe a credentials-file parse failure by path, error category and
/// position only. `serde_json::Error`'s own `Display` quotes the offending
/// value from the file (e.g. `invalid type: string "ldb_..."`), which would
/// put a hand-edited secret on stderr or in a `--json` error envelope, so
/// that message is never formatted here.
fn describe_parse_error(credentials_file: &Path, error: &serde_json::Error) -> String {
    let category = match error.classify() {
        serde_json::error::Category::Io => "io error",
        serde_json::error::Category::Syntax => "invalid syntax",
        serde_json::error::Category::Data => "invalid data",
        serde_json::error::Category::Eof => "unexpected end of input",
    };
    format!(
        "cannot parse {}: {category} at line {} column {}",
        credentials_file.display(),
        error.line(),
        error.column()
    )
}

/// Load the credentials file for a read-only lookup: missing, unreadable or
/// malformed all collapse to "no cached credentials", since a lookup only
/// ever needs to decide whether one exists. Unlike a missing file, an
/// unreadable or malformed one is reported on stderr — it most likely means
/// a previous write was interrupted or the file was hand-edited into an
/// invalid state, and the caller should know even though the lookup itself
/// degrades gracefully.
fn load_file_for_read(credentials_file: &Path) -> CredentialsFile {
    match load_file_for_write(credentials_file) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("{}", read_path_warning(&error));
            CredentialsFile::default()
        }
    }
}

/// The warning text printed for a read-path failure, split out from
/// [`load_file_for_read`] so a test can assert on it directly rather than
/// capturing the process's stderr.
fn read_path_warning(error: &std::io::Error) -> String {
    format!("warning: {error}")
}

/// Check that `credentials_file` can be loaded for a write, without
/// actually writing anything: a missing file is fine (the first write
/// creates it), but an unreadable or malformed one is the same error
/// [`write_entry`] would hit. A caller with work to do before that write —
/// work with a side effect that can't be undone if the write later fails —
/// can call this first and fail before doing any of it.
pub(crate) fn ensure_loadable_for_write(credentials_file: &Path) -> std::io::Result<()> {
    load_file_for_write(credentials_file).map(|_| ())
}

/// Look up the cached entry for `base_url`, if the file exists, parses, and
/// has one.
pub(crate) fn lookup_entry(credentials_file: &Path, base_url: &str) -> Option<CredentialEntry> {
    let file = load_file_for_read(credentials_file);
    file.credentials.get(normalize_base_url(base_url)).cloned()
}

/// Normalize a request URL for prefix matching against a stored
/// credentials-file key: drop any query string or fragment, then a trailing
/// slash, so `https://x/team?foo=1` and `https://x/team/` compare the same
/// way `https://x/team` does.
fn normalize_request_url(url: &str) -> &str {
    let without_fragment = url.split('#').next().unwrap_or(url);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    without_query.trim_end_matches('/')
}

/// Find the stored key that is the longest path-segment prefix of `url`: an
/// exact match, or a stored key immediately followed by `/` in `url` — so a
/// daemon served at `https://example.com/team` matches a request to
/// `https://example.com/team/v1/stores` but not to
/// `https://example.com/teamX/v1`. A daemon served at a bare origin is
/// covered the same way, since the origin is then the whole key. When
/// several stored keys match (e.g. both an origin and a path under it), the
/// longest one wins. Stored keys are already canonical (see
/// [`CredentialsFile::canonicalized`]), so no further trimming is needed
/// here.
fn match_key<'a>(file: &'a CredentialsFile, url: &str) -> Option<&'a str> {
    let url = normalize_request_url(url);
    file.credentials
        .keys()
        .filter(|key| {
            let key = key.as_str();
            url == key || (url.starts_with(key) && url[key.len()..].starts_with('/'))
        })
        .max_by_key(|key| key.len())
        .map(String::as_str)
}

/// Look up the cached entry whose key matches `url` by the longest
/// path-segment prefix (see [`match_key`]), returning the matched key
/// alongside the entry so a caller that refreshes and writes back a token
/// writes under the same key the request matched, not a re-derived one.
pub(crate) fn lookup_entry_for_url(
    credentials_file: &Path,
    url: &str,
) -> Option<(String, CredentialEntry)> {
    let file = load_file_for_read(credentials_file);
    let key = match_key(&file, url)?.to_string();
    let entry = file.credentials.get(&key)?.clone();
    Some((key, entry))
}

/// Look up the cached secret for an exact `base_url` key: an `access_token`
/// (from `localdb login`) is preferred when present, falling back to the
/// legacy `secret` (API key) field. Any failure (missing file, malformed
/// JSON, absent key) is a silent `None`. A test helper for asserting what a
/// known key round-trips to; production code resolves a request's bearer
/// through `daemon_client::fresh_bearer_for_url`'s path-segment-prefix match
/// instead.
#[cfg(test)]
pub(crate) fn lookup_secret(credentials_file: &Path, base_url: &str) -> Option<String> {
    let entry = lookup_entry(credentials_file, base_url)?;
    entry.access_token.or(entry.secret)
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
    let mut file = load_file_for_write(credentials_file)?;
    file.credentials
        .insert(normalize_base_url(base_url).to_string(), entry);
    write_file(credentials_file, &file)
}

/// Remove the entry for `base_url`, if any. Returns `true` if an entry was
/// removed. Same atomic write as [`write_entry`].
pub(crate) fn remove_entry(credentials_file: &Path, base_url: &str) -> std::io::Result<bool> {
    let mut file = load_file_for_write(credentials_file)?;
    let removed = file
        .credentials
        .remove(normalize_base_url(base_url))
        .is_some();
    write_file(credentials_file, &file)?;
    Ok(removed)
}

/// Create the temp file a write replaces `credentials_file` with: a random
/// name (never a predictable `.tmp-<pid>` a pre-planted symlink could
/// follow) and `create_new` (refuse to open through an existing path at
/// all, symlink or otherwise), with `0600` permissions established by the
/// open call itself rather than applied afterward, so the secret is never
/// briefly accessible under whatever mode a pre-existing file would have
/// had.
fn create_temp_file(dir: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    use rand::distributions::Alphanumeric;
    use rand::Rng as _;

    let mut last_error = None;
    for _ in 0..8 {
        let suffix: String = rand::thread_rng()
            .sample_iter(Alphanumeric)
            .take(16)
            .map(char::from)
            .collect();
        let tmp_path = dir.join(format!(".credentials.json.tmp-{suffix}"));

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&tmp_path) {
            Ok(file) => return Ok((tmp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::other("could not create a unique credentials temp file")
    }))
}

fn write_file(credentials_file: &Path, file: &CredentialsFile) -> std::io::Result<()> {
    let dir = credentials_file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(file).unwrap_or_default();

    let (tmp_path, mut tmp_file) = create_temp_file(&dir)?;

    let write_result = tmp_file
        .write_all(json.as_bytes())
        .and_then(|()| tmp_file.sync_all());
    drop(tmp_file);
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error);
    }

    if let Err(error) = std::fs::rename(&tmp_path, credentials_file) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
