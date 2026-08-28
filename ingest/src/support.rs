//! Internal acquisition-side helpers owned by this crate.
//!
//! These serve the concrete ingestors (mime detection from paths,
//! panic-tolerant parsing). They deliberately live here rather than as
//! `pub` items in `core`: core stays free of acquisition concerns, and this
//! crate is the one true implementation. RFC 3339 timestamp formatting is the
//! one exception —
//! `localdb_core::ingestion::format_secs_rfc3339` is `pub` precisely so this
//! crate doesn't need its own copy.

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

#[cfg(test)]
mod format_unix_secs_tests {
    // `ingest` no longer carries its own Gregorian-arithmetic copy (issue
    // callers use `localdb_core::ingestion::format_secs_rfc3339`
    // directly. `epoch_zero_is_1970_01_01`, `leap_day_2024_02_29_is_...`, and
    // `year_end_boundary_rolls_over_correctly` exactly duplicated core's own
    // golden tests (`core::ingestion::format_secs_rfc3339_tests`) and are not
    // re-ported here. `mid_2026_value_is_formatted_correctly` covered a value
    // core's golden tests don't, so it's kept.
    use localdb_core::ingestion::format_secs_rfc3339;

    #[test]
    fn mid_2026_value_is_formatted_correctly() {
        assert_eq!(format_secs_rfc3339(1_783_524_645), "2026-07-08T15:30:45Z");
    }
}

/// Test doubles shared by `file_ingestor` and `url_ingestor` unit tests.
#[cfg(test)]
pub(crate) mod test_doubles {
    use localdb_core::block::Resource;
    use localdb_core::error::Error;
    use localdb_core::ingestor::{IngestCallback, SkipReason};
    use localdb_core::uri::Uri;

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

        async fn on_skipped(&mut self, uri: &Uri, reason: SkipReason) {
            self.skipped.push((uri.as_str().to_string(), reason));
        }
    }
}
