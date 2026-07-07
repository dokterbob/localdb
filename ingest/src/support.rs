//! Internal helpers mirrored from `core::ingestion`.
//!
//! `core::ingestion::{catch_panic, detect_mime, format_unix_secs, secs_to_ymd_hms}`
//! are private to that module, so they are copied here rather than making
//! them `pub` in `core` (out of scope for this crate: `core` is owned by a
//! concurrent workstream). Keep these in sync by hand until issue #117's
//! later wave deletes `core`'s copies of the file/URL ingestors and this
//! becomes the one true implementation.

use std::path::Path;

/// Run a fallible-by-panic closure and turn any panic into a plain message,
/// suppressing the default panic hook's stderr spew for the duration.
///
/// Mirrors the *mechanism* of `core::ingestion::catch_panic` (temporarily
/// replacing the panic hook, `catch_unwind`, restoring the hook) but returns
/// `Result<T, String>` instead of folding the panic into `Error::Internal`.
/// That keeps "the parser panicked" unambiguous at call sites from "the
/// parser returned a real `Err`", which core's version — folding both into
/// `Error::Internal` — does not, since callers there only ever see one Err
/// arm either way.
///
/// # Thread safety
/// The panic hook is a process-global; swapping it is **not** thread-safe.
/// Callers must ensure no concurrent `catch_panic` calls happen. Ingestors in
/// this crate process items sequentially, so this holds.
pub(crate) fn catch_panic<T>(f: impl FnOnce() -> T + std::panic::UnwindSafe) -> Result<T, String> {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(f);
    std::panic::set_hook(prev_hook);

    result.map_err(|payload| {
        payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic payload".to_string())
    })
}

/// Simple MIME type detection from file extension.
///
/// Verbatim copy of `core::ingestion::detect_mime` (private there). Used for
/// the stored `Resource.mime` field on file-sourced resources — distinct from
/// `extract::sniff_mime`, which is advisory input to the parser chain itself.
pub(crate) fn detect_mime(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?;
    Some(
        match ext.to_lowercase().as_str() {
            "md" | "markdown" => "text/markdown",
            "txt" => "text/plain",
            "html" | "htm" => "text/html",
            "pdf" => "application/pdf",
            "epub" => "application/epub+zip",
            "rs" => "text/x-rust",
            "py" => "text/x-python",
            "js" | "mjs" => "text/javascript",
            "ts" | "tsx" => "text/typescript",
            "json" => "application/json",
            "yaml" | "yml" => "text/yaml",
            "toml" => "text/toml",
            _ => "application/octet-stream",
        }
        .to_string(),
    )
}

/// Format a Unix timestamp as RFC 3339 (UTC, no sub-second precision).
///
/// Verbatim copy of `core::ingestion::format_unix_secs` (private there),
/// including its test-cfg fixed-string shortcut, so ingest's own tests get
/// the same deterministic timestamps core's ingestion tests rely on.
pub(crate) fn format_unix_secs(secs: u64) -> String {
    #[cfg(not(test))]
    {
        let (y, mo, d, h, mi, s) = secs_to_ymd_hms(secs);
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, mi, s)
    }
    #[cfg(test)]
    {
        let _ = secs;
        "2026-06-10T12:00:00Z".to_string()
    }
}

/// Civil (Gregorian) date/time decomposition of a Unix timestamp.
///
/// Verbatim copy of `core::ingestion::secs_to_ymd_hms` (private there).
#[cfg(not(test))]
fn secs_to_ymd_hms(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;

    // Gregorian calendar calculation
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y_adj = if mo <= 2 { y + 1 } else { y };

    (y_adj, mo, d, h, m, s)
}

/// Test doubles shared by `file_ingestor` and `url_ingestor` unit tests.
#[cfg(test)]
pub(crate) mod test_doubles {
    use localdb_core::block::Resource;
    use localdb_core::error::Error;
    use localdb_core::ingestor::{IngestCallback, SkipReason};

    /// Records every callback invocation for assertions, instead of silently
    /// dropping progress signals the way a minimal fake normally would.
    #[derive(Default)]
    pub(crate) struct RecordingCallback {
        pub resources: Vec<Resource>,
        pub discovered: Vec<usize>,
        pub skipped: Vec<(String, SkipReason)>,
    }

    #[async_trait::async_trait]
    impl IngestCallback for RecordingCallback {
        async fn on_resource(&mut self, resource: Resource) -> Result<(), Error> {
            self.resources.push(resource);
            Ok(())
        }

        async fn on_discovered(&mut self, total: usize) {
            self.discovered.push(total);
        }

        async fn on_skipped(&mut self, uri: &str, reason: SkipReason) {
            self.skipped.push((uri.to_string(), reason));
        }
    }
}
