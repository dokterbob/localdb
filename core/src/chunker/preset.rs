//! `preset_for` — classify a file as "code" or "prose".

/// Classify a file as `"code"` or `"prose"` based on MIME type and filename.
///
/// Logic: check mime first, then filename basename for lockfiles, then extension.
/// Falls back to `"prose"` if nothing matches.
pub fn preset_for(filename: Option<&str>, mime: Option<&str>) -> &'static str {
    const CODE_EXTS: &[&str] = &[
        "rs", "py", "js", "mjs", "ts", "tsx", "json", "yaml", "yml", "toml", "lock", "c", "h",
        "cpp", "hpp", "go", "java", "rb", "php", "sh", "css", "scss", "sql", "csv", "xml", "ini",
        "cfg", "xlsx", "xls",
    ];
    const PROSE_EXTS: &[&str] = &["md", "markdown", "html", "htm", "pdf", "txt", "text"];
    const LOCKFILE_BASENAMES: &[&str] = &[
        "Cargo.lock",
        "package-lock.json",
        "yarn.lock",
        "poetry.lock",
        "Gemfile.lock",
    ];

    // Check MIME first.
    if let Some(m) = mime {
        if m == "application/json" || m.starts_with("text/x-") {
            return "code";
        }
        if m == "text/plain" {
            return "prose";
        }
    }

    // Check lockfile basenames (case-sensitive).
    if let Some(name) = filename {
        let basename = std::path::Path::new(name)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(name);
        if LOCKFILE_BASENAMES.contains(&basename) {
            return "code";
        }

        // Check extension (case-insensitive).
        let ext = std::path::Path::new(name)
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_lowercase());
        if let Some(ext) = ext {
            if CODE_EXTS.contains(&ext.as_str()) {
                return "code";
            }
            if PROSE_EXTS.contains(&ext.as_str()) {
                return "prose";
            }
        }
    }

    // Default: prose (safe fallback)
    "prose"
}
