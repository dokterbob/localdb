//! Plain-text parser: accepts known code/data file extensions and prose text files.
//!
//! Only accepts files with recognized extensions (code, data, or plain text).
//! Place this **last** in the chain so that more specific parsers (Office, HTML,
//! Markdown, PDF) get first pick.

use localdb_core::parser::{DocumentMetadata, ParsedDocument, Parser, Probe};
use localdb_core::Error;

/// Recognized extensions for the plaintext parser (prose text).
const PROSE_EXTS: &[&str] = &["txt", "text"];

/// Recognized extensions for the plaintext parser (code/data files).
const CODE_EXTS: &[&str] = &[
    "rs", "py", "js", "mjs", "ts", "tsx", "json", "yaml", "yml", "toml", "lock", "c", "h", "cpp",
    "hpp", "go", "java", "rb", "php", "sh", "css", "scss", "sql", "csv", "xml", "ini", "cfg",
];

/// Lockfile basenames that are always accepted regardless of extension.
const LOCKFILE_BASENAMES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "poetry.lock",
    "Gemfile.lock",
];

/// Returns `true` if the filename has a recognized extension or is a known lockfile basename.
fn is_supported_filename(filename: Option<&str>) -> bool {
    let Some(name) = filename else { return false };
    // Check lockfile basenames (case-sensitive for basenames like Cargo.lock).
    let basename = std::path::Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    if LOCKFILE_BASENAMES.contains(&basename) {
        return true;
    }
    // Check extension (case-insensitive).
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_lowercase());
    if let Some(ext) = &ext {
        if PROSE_EXTS.contains(&ext.as_str()) || CODE_EXTS.contains(&ext.as_str()) {
            return true;
        }
    }
    false
}

/// Parser for plain text and code/data files with recognized extensions.
///
/// Returns `Ok(None)` for:
/// - Filenames without a recognized extension.
/// - Missing/unknown filenames (no path_hint).
/// - Non-UTF-8 bytes.
pub struct PlaintextParser;

impl Parser for PlaintextParser {
    fn id(&self) -> &'static str {
        "plaintext"
    }

    fn parse(&self, probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
        // Only accept known extensions; decline unknown/missing extension → UnsupportedFormat.
        if !is_supported_filename(probe.path_hint) {
            return Ok(None);
        }

        let text = match std::str::from_utf8(probe.bytes()) {
            Ok(t) => t,
            Err(_) => return Ok(None),
        };

        let (markdown, title) = crate::plaintext::extract_plaintext(text)?;

        let mut dc = DocumentMetadata::default();
        if let Some(mime) = probe.sniffed_mime {
            dc.format = Some(mime.to_string());
        }

        Ok(Some(ParsedDocument {
            markdown,
            title,
            metadata: dc,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::parser::Probe;

    #[test]
    fn accepts_utf8_text() {
        let probe = Probe::new(b"Hello, world!", Some("hello.txt"), None);
        let doc = PlaintextParser.parse(&probe).unwrap().unwrap();
        assert!(doc.markdown.contains("Hello"));
    }

    #[test]
    fn declines_binary_non_utf8() {
        let binary = b"\xFF\xFE\x00\x01binary content";
        let probe = Probe::new(binary, Some("file.txt"), None);
        assert!(PlaintextParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn accepts_recognized_extensions() {
        for ext in &["txt", "rs", "json"] {
            let filename = format!("file.{ext}");
            let probe = Probe::new(b"Some content", Some(&filename), None);
            assert!(
                PlaintextParser.parse(&probe).unwrap().is_some(),
                "PlaintextParser should accept .{ext}"
            );
        }
    }

    #[test]
    fn declines_no_extension_filename() {
        let probe = Probe::new(b"Plain text content", Some("Makefile"), None);
        assert!(PlaintextParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn sniffed_mime_populates_dc_format() {
        let probe = Probe::new(b"plain text", Some("notes.txt"), Some("text/plain"));
        let doc = PlaintextParser.parse(&probe).unwrap().unwrap();
        assert_eq!(doc.metadata.format, Some("text/plain".to_string()));
    }

    #[test]
    fn accepts_rs_extension() {
        let probe = Probe::new(b"fn main() {}", Some("main.rs"), None);
        assert!(PlaintextParser.parse(&probe).unwrap().is_some());
    }

    #[test]
    fn accepts_json_extension() {
        let probe = Probe::new(b"{}", Some("config.json"), None);
        assert!(PlaintextParser.parse(&probe).unwrap().is_some());
    }

    #[test]
    fn accepts_txt_extension() {
        let probe = Probe::new(b"plain text", Some("notes.txt"), None);
        assert!(PlaintextParser.parse(&probe).unwrap().is_some());
    }

    #[test]
    fn declines_bin_extension() {
        let probe = Probe::new(b"some bytes", Some("binary.bin"), None);
        assert!(PlaintextParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn declines_xyz_extension() {
        let probe = Probe::new(b"data", Some("file.xyz"), None);
        assert!(PlaintextParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn declines_no_filename() {
        let probe = Probe::new(b"data", None, None);
        assert!(PlaintextParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn declines_no_extension() {
        let probe = Probe::new(b"data", Some("Makefile"), None);
        assert!(PlaintextParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn accepts_lockfile_basename() {
        let probe = Probe::new(b"[[package]]", Some("Cargo.lock"), None);
        assert!(PlaintextParser.parse(&probe).unwrap().is_some());
    }

    // ---------------------------------------------------------------------------
    // Regression tests: unknown/binary extensions must be declined (UnsupportedFormat)
    // ---------------------------------------------------------------------------

    /// Regression: a `.bin` file with valid UTF-8 must be declined (Ok(None)) by
    /// PlaintextParser, not accepted. Before the fix, PlaintextParser accepted any
    /// UTF-8 content regardless of extension, causing unknown files to be indexed
    /// via the slow prose chunker.
    #[test]
    fn regression_unknown_extension_is_unsupported() {
        let probe = Probe::new(b"some utf-8 content here", Some("data.bin"), None);
        let result = PlaintextParser.parse(&probe).unwrap();
        assert!(
            result.is_none(),
            "data.bin must be declined (Ok(None)) by PlaintextParser"
        );
    }

    /// Regression: a file with no extension must also be declined (Ok(None)).
    /// Before the fix, extensionless files with valid UTF-8 were accepted and
    /// sent to the prose chunker.
    #[test]
    fn regression_no_extension_is_unsupported() {
        let probe = Probe::new(b"#!/usr/bin/env bash\necho hello", Some("somefile"), None);
        let result = PlaintextParser.parse(&probe).unwrap();
        assert!(
            result.is_none(),
            "extensionless 'somefile' must be declined (Ok(None)) by PlaintextParser"
        );
    }

    /// Regression: a `.rs` file must be accepted — supported Rust source files
    /// should still be indexed after the extension-gating fix.
    #[test]
    fn regression_rs_file_is_supported() {
        let probe = Probe::new(
            b"fn main() {\n    println!(\"hello\");\n}\n",
            Some("main.rs"),
            None,
        );
        let result = PlaintextParser.parse(&probe).unwrap();
        assert!(
            result.is_some(),
            "main.rs must be accepted (Ok(Some(_))) by PlaintextParser"
        );
    }
}
