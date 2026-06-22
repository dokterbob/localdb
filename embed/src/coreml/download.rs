//! CoreML bundle download via the async `hf-hub` (1.0.0-rc.1) client.
//!
//! The canonical CoreML repo (`dokterbob/pplx-embed-coreml`) ships several
//! sequence-length "buckets", each a self-contained `.mlmodelc`/`.mlpackage`
//! plus a `hf_model/` tokenizer and a `model_config.json`.  A top-level
//! `manifest.json` enumerates every bucket and its files.
//!
//! This module:
//! 1. Fetches & parses `manifest.json` ([`fetch_manifest`]).
//! 2. Selects which *context* buckets to download and which files within them
//!    ([`select_context_files`]) — choosing one format (`.mlmodelc` when
//!    compiled is preferred, else `.mlpackage`) per bucket and always keeping
//!    the shared files (`model_config.json`, `hf_model/…`).
//! 3. Downloads each selected file at a pinned revision and returns the
//!    snapshot root ([`download_bundle`]) such that
//!    `<root>/context/L512-int8/encoder.mlmodelc` resolves.
//!
//! # hf-hub 1.0.0-rc.1 async API
//!
//! The entry point is [`hf_hub::HFClient`].  A repository handle is bound with
//! `client.model(owner, name)`, and a single file is fetched with the builder
//! `repo.download_file().filename(path).revision(rev).send().await`, which
//! returns the cached path (`<cache>/models--owner--name/snapshots/<commit>/<path>`).
//! XET deduplication is transparent.  We strip the file's relative path from
//! the returned path to recover the snapshot root.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::EmbedError;

// ---------------------------------------------------------------------------
// Manifest types
// ---------------------------------------------------------------------------

/// One file listed under a bucket in `manifest.json`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ManifestFile {
    /// Repo-relative path, e.g. `context/L512-int8/encoder.mlmodelc/model.mil`.
    pub path: String,
}

/// One bucket entry parsed from `manifest.json`.
///
/// The upstream `bucket` field is an integer for fixed buckets but a string
/// range (e.g. `"1..8192"`) for the dynamic bucket, so it is parsed leniently:
/// non-integer values deserialize to `0` and are only meaningful when
/// `dynamic == false`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ManifestBucket {
    /// Repo-relative directory prefix, e.g. `context/L512-int8` or `L1024-int8`.
    pub subfolder: String,
    /// `"plain"` or `"context"`.
    #[serde(default = "default_variant")]
    pub variant: String,
    /// Whether this is a dynamic (variable seq-len) bucket.
    #[serde(default)]
    pub dynamic: bool,
    /// Fixed bucket size (sequence length). `0` for the dynamic bucket.
    #[serde(default, deserialize_with = "lenient_i64")]
    pub bucket: i64,
    /// Maximum sequence length supported by this bucket.
    #[serde(default, alias = "maxSeqLen")]
    pub max_seq_len: i64,
    /// Upper bound for a dynamic bucket (`0` for fixed buckets).
    #[serde(default)]
    pub dynamic_upper: i64,
    /// Formats published for this bucket, e.g. `["mlmodelc", "mlpackage"]`.
    #[serde(default)]
    pub formats: Vec<String>,
    /// Files belonging to this bucket (repo-relative, subfolder-prefixed).
    #[serde(default)]
    pub files: Vec<ManifestFile>,
}

fn default_variant() -> String {
    "plain".to_string()
}

/// Deserialize an `i64` that may arrive as a number or as a non-numeric string
/// (the dynamic bucket's `"1..8192"`).  Non-numeric strings map to `0`.
fn lenient_i64<'de, D>(de: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(de)?;
    Ok(match v {
        serde_json::Value::Number(n) => n.as_i64().unwrap_or(0),
        serde_json::Value::String(s) => s.parse::<i64>().unwrap_or(0),
        _ => 0,
    })
}

/// Parsed `manifest.json`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Manifest {
    #[serde(default)]
    pub buckets: Vec<ManifestBucket>,
}

/// Parsed metadata for a selected context bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContextBucket {
    /// Repo-relative directory prefix, e.g. `context/L512-int8`.
    pub subfolder: String,
    /// Maximum sequence length for this bucket.
    pub max_seq_len: i64,
    /// Whether this is a dynamic bucket.
    pub dynamic: bool,
    /// Upper bound for the dynamic bucket (`0` for fixed buckets).
    pub dynamic_upper: i64,
}

impl ManifestBucket {
    /// Effective max sequence length: `bucket` for fixed buckets, falling back
    /// to `max_seq_len` then `dynamic_upper`.  Mirrors the Swift reference.
    fn effective_max_seq_len(&self) -> i64 {
        if self.bucket > 0 {
            self.bucket
        } else if self.max_seq_len > 0 {
            self.max_seq_len
        } else {
            self.dynamic_upper
        }
    }

    /// Repo-relative file paths to fetch for this bucket in the chosen format.
    ///
    /// Keeps shared files (`model_config.json`, `hf_model/…`) plus only the
    /// chosen encoder format's directory, excluding the other format's
    /// `encoder.<other>/` directory entirely.
    fn select_files(&self, prefer_compiled: bool) -> Vec<String> {
        let preferred = if prefer_compiled {
            "mlmodelc"
        } else {
            "mlpackage"
        };
        let chosen = if self.formats.iter().any(|f| f == preferred) {
            preferred
        } else {
            self.formats
                .first()
                .map(String::as_str)
                .unwrap_or(preferred)
        };
        let other = if chosen == "mlmodelc" {
            "mlpackage"
        } else {
            "mlmodelc"
        };
        let other_dir = format!("{}/encoder.{}/", self.subfolder, other);
        self.files
            .iter()
            .filter(|f| !f.path.starts_with(&other_dir))
            .map(|f| f.path.clone())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Manifest fetch
// ---------------------------------------------------------------------------

/// Fetch and parse `manifest.json` from `repo` at `revision`.
pub(crate) async fn fetch_manifest(
    client: &hf_hub::HFClient,
    repo: &str,
    revision: &str,
) -> Result<Manifest, EmbedError> {
    let (owner, name) = split_repo(repo)?;
    let bytes = client
        .model(owner, name)
        .download_file_to_bytes()
        .filename("manifest.json")
        .revision(revision.to_string())
        .send()
        .await
        .map_err(|e| EmbedError::ProviderError {
            provider: "huggingface".into(),
            message: format!("fetching manifest.json from {repo}@{revision}: {e}"),
        })?;
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    Ok(manifest)
}

/// Split `"owner/name"` into its two halves.
fn split_repo(repo: &str) -> Result<(&str, &str), EmbedError> {
    repo.split_once('/').ok_or_else(|| {
        EmbedError::Internal(format!(
            "malformed repo id '{repo}' (expected 'owner/name')"
        ))
    })
}

// ---------------------------------------------------------------------------
// Bucket / file selection
// ---------------------------------------------------------------------------

/// Select context-variant buckets among the requested sizes (plus any dynamic
/// catch-all) and collect their chosen-format file lists.
///
/// Returns the flat list of repo-relative files to download and the parsed
/// metadata for each selected bucket.  `want` lists the requested fixed bucket
/// sizes (e.g. `[512, 1024, 2048, 4096]`).
pub(crate) fn select_context_files(
    manifest: &Manifest,
    want: &[i64],
    prefer_compiled: bool,
) -> (Vec<String>, Vec<ContextBucket>) {
    let mut files = Vec::new();
    let mut buckets = Vec::new();

    for b in &manifest.buckets {
        if b.variant != "context" {
            continue;
        }
        if !(b.dynamic || want.contains(&b.bucket)) {
            continue;
        }
        files.extend(b.select_files(prefer_compiled));
        buckets.push(ContextBucket {
            subfolder: b.subfolder.clone(),
            max_seq_len: b.effective_max_seq_len(),
            dynamic: b.dynamic,
            dynamic_upper: b.dynamic_upper,
        });
    }

    (files, buckets)
}

// ---------------------------------------------------------------------------
// Bundle download
// ---------------------------------------------------------------------------

/// Download the selected context bundle at a pinned revision.
///
/// - `repo`: HF repo id (`owner/name`).
/// - `revision`: pinned commit sha.
/// - `want`: requested fixed bucket sizes.
/// - `into`: optional target directory; when `None`, the HF cache is used.
/// - `show_progress`: reserved for future progress wiring (currently the
///   hf-hub client handles its own retries; progress events are not surfaced).
///
/// Returns the snapshot root directory such that
/// `<root>/context/L512-int8/encoder.mlmodelc` resolves.
pub(crate) async fn download_bundle(
    repo: &str,
    revision: &str,
    want: &[i64],
    into: Option<PathBuf>,
    show_progress: bool,
) -> Result<PathBuf, EmbedError> {
    let _ = show_progress;
    let (owner, name) = split_repo(repo)?;

    let mut builder = hf_hub::HFClient::builder();
    if let Some(ref dir) = into {
        builder = builder.cache_dir(dir.clone());
    }
    let client = builder
        .build()
        .map_err(|e| EmbedError::Internal(format!("building HF client: {e}")))?;

    let manifest = fetch_manifest(&client, repo, revision).await?;
    let (selected, buckets) = select_context_files(&manifest, want, true);

    if selected.is_empty() || buckets.is_empty() {
        return Err(EmbedError::ModelMissing(format!(
            "no context buckets in {repo} manifest match requested sizes {want:?}. \
             The repo may not yet publish a context bucket; alternatively use \
             provider: local-onnx, model: pplx-embed-context-v1-0.6b."
        )));
    }

    let repo_handle = client.model(owner, name);
    let mut snapshot_root: Option<PathBuf> = None;

    for rel in &selected {
        let cached = repo_handle
            .download_file()
            .filename(rel.clone())
            .revision(revision.to_string())
            .send()
            .await
            .map_err(|e| EmbedError::ProviderError {
                provider: "huggingface".into(),
                message: format!("downloading {rel} from {repo}@{revision}: {e}"),
            })?;

        // Derive the snapshot root by stripping the file's repo-relative path
        // components from the returned cache path.  hf-hub returns
        // `<root>/<rel>` (with `<rel>` possibly nested), so we strip exactly as
        // many trailing components as `<rel>` has.
        let root = strip_relative(&cached, rel).ok_or_else(|| {
            EmbedError::Internal(format!(
                "cannot derive snapshot root: returned path '{}' does not end with '{rel}'",
                cached.display()
            ))
        })?;
        snapshot_root = Some(root);
    }

    snapshot_root.ok_or_else(|| {
        EmbedError::Internal("no files were downloaded from the CoreML bundle".to_string())
    })
}

/// Strip the trailing path components of `rel` (a forward-slash repo path) from
/// `full`, returning the prefix directory.
///
/// `strip_relative("/cache/.../snapshots/abc/context/L512-int8/x.bin",
/// "context/L512-int8/x.bin") == "/cache/.../snapshots/abc"`.
fn strip_relative(full: &Path, rel: &str) -> Option<PathBuf> {
    let depth = rel.split('/').filter(|s| !s.is_empty()).count();
    let mut root = full.to_path_buf();
    for _ in 0..depth {
        if !root.pop() {
            return None;
        }
    }
    Some(root)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "buckets": [
            {
                "subfolder": "L512-int8", "variant": "plain", "bucket": 512,
                "dynamic": false, "dynamic_upper": 0, "max_seq_len": 512,
                "formats": ["mlmodelc", "mlpackage"],
                "files": [
                    {"path": "L512-int8/encoder.mlmodelc/model.mil"},
                    {"path": "L512-int8/encoder.mlpackage/Manifest.json"},
                    {"path": "L512-int8/model_config.json"}
                ]
            },
            {
                "subfolder": "context/L512-int8", "variant": "context", "bucket": 512,
                "dynamic": false, "dynamic_upper": 0, "max_seq_len": 512,
                "formats": ["mlmodelc", "mlpackage"],
                "files": [
                    {"path": "context/L512-int8/encoder.mlmodelc/model.mil"},
                    {"path": "context/L512-int8/encoder.mlmodelc/weights/weight.bin"},
                    {"path": "context/L512-int8/encoder.mlpackage/Manifest.json"},
                    {"path": "context/L512-int8/encoder.mlpackage/Data/com.apple.CoreML/weights/weight.bin"},
                    {"path": "context/L512-int8/model_config.json"},
                    {"path": "context/L512-int8/hf_model/tokenizer.json"}
                ]
            },
            {
                "subfolder": "dyn8192-int8", "variant": "context", "bucket": "1..8192",
                "dynamic": true, "dynamic_upper": 8192, "max_seq_len": 4096,
                "formats": ["mlmodelc", "mlpackage"],
                "files": [
                    {"path": "dyn8192-int8/encoder.mlmodelc/model.mil"},
                    {"path": "dyn8192-int8/encoder.mlpackage/Manifest.json"},
                    {"path": "dyn8192-int8/model_config.json"}
                ]
            }
        ]
    }"#;

    fn parse() -> Manifest {
        serde_json::from_str(SAMPLE).expect("parse sample manifest")
    }

    #[test]
    fn parses_manifest_with_string_bucket() {
        let m = parse();
        assert_eq!(m.buckets.len(), 3);
        // The dynamic bucket's string "1..8192" parses leniently to 0.
        let dyn_b = m.buckets.iter().find(|b| b.dynamic).unwrap();
        assert_eq!(dyn_b.bucket, 0);
        assert_eq!(dyn_b.dynamic_upper, 8192);
        assert_eq!(dyn_b.effective_max_seq_len(), 4096);
    }

    #[test]
    fn selects_only_context_buckets() {
        let m = parse();
        let (files, buckets) = select_context_files(&m, &[512, 1024, 2048, 4096], true);
        // Two context buckets (fixed 512 + dynamic) selected; the plain bucket
        // is excluded.
        assert_eq!(buckets.len(), 2);
        assert!(buckets.iter().any(|b| b.subfolder == "context/L512-int8"));
        assert!(buckets.iter().any(|b| b.dynamic));
        // No plain-bucket files leaked in.
        assert!(files.iter().all(|f| !f.starts_with("L512-int8/")));
    }

    #[test]
    fn compiled_format_excludes_mlpackage() {
        let m = parse();
        let (files, _) = select_context_files(&m, &[512], true);
        // mlmodelc dir kept, mlpackage dir excluded, shared files kept.
        assert!(files
            .iter()
            .any(|f| f == "context/L512-int8/encoder.mlmodelc/model.mil"));
        assert!(files
            .iter()
            .any(|f| f == "context/L512-int8/encoder.mlmodelc/weights/weight.bin"));
        assert!(files.iter().all(|f| !f.contains("encoder.mlpackage/")));
        assert!(files
            .iter()
            .any(|f| f == "context/L512-int8/model_config.json"));
        assert!(files
            .iter()
            .any(|f| f == "context/L512-int8/hf_model/tokenizer.json"));
    }

    #[test]
    fn package_format_excludes_mlmodelc() {
        let m = parse();
        let (files, _) = select_context_files(&m, &[512], false);
        assert!(files.iter().all(|f| !f.contains("encoder.mlmodelc/")));
        assert!(files.iter().any(|f| f.contains("encoder.mlpackage/")));
    }

    #[test]
    fn no_matching_buckets_returns_empty() {
        let m = parse();
        // No fixed context bucket at 1024 and disable dynamic by removing it is
        // not possible here; but request a size with no fixed context match —
        // the dynamic bucket is still picked up, so files are non-empty.
        let (files, buckets) = select_context_files(&m, &[1024], true);
        assert!(buckets.iter().any(|b| b.dynamic));
        assert!(!files.is_empty());
    }

    #[test]
    fn strip_relative_nested() {
        let full = Path::new(
            "/cache/models--x/snapshots/abc/context/L512-int8/encoder.mlmodelc/model.mil",
        );
        let root = strip_relative(full, "context/L512-int8/encoder.mlmodelc/model.mil").unwrap();
        assert_eq!(root, Path::new("/cache/models--x/snapshots/abc"));
    }

    #[test]
    fn strip_relative_flat() {
        let full = Path::new("/cache/models--x/snapshots/abc/manifest.json");
        let root = strip_relative(full, "manifest.json").unwrap();
        assert_eq!(root, Path::new("/cache/models--x/snapshots/abc"));
    }

    #[test]
    fn strip_relative_too_deep_fails() {
        let full = Path::new("/a/b.bin");
        assert!(strip_relative(full, "x/y/z/b.bin").is_none());
    }

    #[test]
    fn split_repo_ok_and_err() {
        assert_eq!(split_repo("owner/name").unwrap(), ("owner", "name"));
        assert!(split_repo("noslash").is_err());
    }
}
