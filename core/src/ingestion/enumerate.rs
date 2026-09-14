//! `path`-kind source enumeration: walk a configured root, apply
//! include/exclude globs, and report what a `FileIngestor` should treat as
//! the source's current contents.
//!
//! Kept separate from the pipeline proper (`super::pipeline`) and from the
//! feed liveness sweep (`super::liveness`): this module only discovers what
//! exists on disk for a `path` source and never touches a store, an
//! embedder, or `DocumentIndex` — the ingestor in the `ingest` crate is the
//! only caller.

use std::path::Path;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::error::Error;
use crate::uri::Uri;

/// A file found by path-source enumeration.
#[derive(Debug, Clone)]
pub struct FoundFile {
    /// Absolute file path.
    pub path: std::path::PathBuf,
    /// Canonical file URI: `file:///absolute/path`.
    pub uri: Uri,
}

/// The outcome of enumerating a `path`-kind source.
///
/// This is an enum rather than a plain `Vec<FoundFile>` on purpose (#156):
/// a missing root used to be flattened into `Ok(vec![])`, indistinguishable
/// from an empty-but-present directory, and the delete-sweep read that empty
/// vector as "every file in this source was deleted." Making the caller
/// destructure the two cases is the fix — every future caller has to confront
/// the distinction that caused the data loss.
#[derive(Debug, Clone)]
pub enum PathEnumeration {
    /// The root was present and walked in full: these are all its files.
    Complete(Vec<FoundFile>),
    /// The root does not exist — an unmounted volume, a detached external
    /// disk, a moved directory. Says nothing about whether the files it used
    /// to hold still exist, so it must never license a delete.
    RootUnavailable,
}

impl PathEnumeration {
    /// The enumerated files, or an empty slice if the root was unavailable.
    ///
    /// Convenience for callers that only care about what was found (tests,
    /// display). Anything that *deletes* on the strength of absence must
    /// match on the variant instead.
    pub fn files(&self) -> &[FoundFile] {
        match self {
            PathEnumeration::Complete(files) => files,
            PathEnumeration::RootUnavailable => &[],
        }
    }
}

/// Enumerate files in a `path`-kind source, applying include/exclude globs.
///
/// Returns [`PathEnumeration::Complete`] with the found files sorted by path
/// for determinism, or [`PathEnumeration::RootUnavailable`] if the configured
/// root does not exist.
///
/// # Errors
/// Returns `Error::Internal` if the root path exists but cannot be read.
pub fn enumerate_path_source(
    root: &str,
    include: &[String],
    exclude: &[String],
) -> Result<PathEnumeration, Error> {
    let root_path = Path::new(root);

    if !root_path.exists() {
        // #156: a root that isn't there is *unavailable*, not empty. Reporting
        // it as zero files is what let an unmounted volume delete a whole
        // source's worth of indexed documents.
        return Ok(PathEnumeration::RootUnavailable);
    }

    let include_set = build_glob_set(include)?;
    let exclude_set = build_glob_set(exclude)?;
    let include_empty = include.is_empty();

    let mut found = Vec::new();
    enumerate_dir(
        root_path,
        root_path,
        &include_set,
        include_empty,
        &exclude_set,
        &mut found,
    )?;
    found.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(PathEnumeration::Complete(found))
}

/// Recursively enumerate a directory.
fn enumerate_dir(
    root: &Path,
    dir: &Path,
    include_set: &GlobSet,
    include_empty: bool,
    exclude_set: &GlobSet,
    found: &mut Vec<FoundFile>,
) -> Result<(), Error> {
    let entries = std::fs::read_dir(dir).map_err(|e| Error::Internal {
        message: format!("cannot read directory '{}': {}", dir.display(), e),
        correlation_id: "enumerate_dir".to_string(),
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| Error::Internal {
            message: format!("error reading directory entry: {}", e),
            correlation_id: "enumerate_dir_entry".to_string(),
        })?;

        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let relative_str = relative.to_string_lossy();

        // Apply exclude globs first. Match the root-relative path (so anchored
        // patterns like `**/node_modules/**` work) AND the bare file/dir name (so
        // a bare pattern like `.DS_Store` matches at any depth, e.g.
        // `Call/.DS_Store`). The include check below intentionally stays
        // path-anchored.
        if let Some(name) = path.file_name() {
            let basename = name.to_string_lossy();
            if exclude_set.is_match(relative_str.as_ref())
                || exclude_set.is_match(basename.as_ref())
            {
                continue;
            }
        } else if exclude_set.is_match(relative_str.as_ref()) {
            continue;
        }

        if path.is_dir() {
            enumerate_dir(root, &path, include_set, include_empty, exclude_set, found)?;
        } else if path.is_file() {
            // Apply include globs: if any are specified, file must match one
            if !include_empty && !include_set.is_match(relative_str.as_ref()) {
                continue;
            }

            let abs_path = path.canonicalize().unwrap_or(path.clone());
            // `Uri::from_file_path` percent-encodes correctly (spaces,
            // non-ASCII, `#`, `?`, ...), unlike the old lossy
            // `format!("file://{}", path.display())`. It returns `None` only
            // for a non-absolute path, which `abs_path` is not — *unless*
            // `canonicalize()` above failed (the file was moved or deleted
            // between `is_file()` and here) and the source's configured root
            // was itself relative, which `normalize_path_source` permits.
            //
            // Error out rather than panicking or silently dropping the file.
            // Dropping it would be the worse of the two: the file would never
            // be reported to the pipeline, so the delete-sweep would treat its
            // still-live document as gone and delete it — exactly the data
            // loss this module's normalization work exists to prevent.
            // Returning `Err` aborts the run before the sweep, so nothing is
            // deleted on the strength of an incomplete enumeration.
            let uri = Uri::from_file_path(&abs_path).ok_or_else(|| Error::Internal {
                message: format!(
                    "cannot build a file:// URI for non-absolute path '{}' \
                     (canonicalization failed and the source root is relative)",
                    abs_path.display()
                ),
                correlation_id: "enumerate_dir".to_string(),
            })?;
            found.push(FoundFile {
                path: abs_path,
                uri,
            });
        }
    }

    Ok(())
}

/// Build a compiled `GlobSet` from a slice of glob pattern strings.
///
/// Each pattern is compiled with `literal_separator(true)` so that `*` and `?`
/// do not cross `/`, while `**` still matches across directory boundaries —
/// matching the pre-existing semantics exactly.
fn build_glob_set(patterns: &[String]) -> Result<GlobSet, Error> {
    let mut b = GlobSetBuilder::new();
    for pat in patterns {
        let glob = GlobBuilder::new(pat)
            .literal_separator(true)
            .build()
            .map_err(|e| Error::InvalidConfig {
                message: format!("invalid glob pattern '{pat}': {e}"),
            })?;
        b.add(glob);
    }
    b.build().map_err(|e| Error::InvalidConfig {
        message: format!("failed to build glob set: {e}"),
    })
}

/// Thin wrapper used only by unit tests: match a single pattern against a path.
///
/// `pub(in crate::ingestion)`, not private: its tests live in the sibling
/// `ingestion::tests` module, which needs to reach it despite not being a
/// descendant of this module.
#[cfg(test)]
pub(in crate::ingestion) fn glob_match(pattern: &str, path: &str) -> bool {
    let Ok(set) = build_glob_set(&[pattern.to_string()]) else {
        return false;
    };
    set.is_match(path)
}
