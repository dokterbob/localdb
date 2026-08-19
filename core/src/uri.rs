use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Canonical resource locator wrapping `url::Url`.
///
/// Accepts any valid URL including `file://` paths, `https://` URLs, and
/// connector-defined schemes (e.g. `notion://`, `telegram://`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Uri(url::Url);

impl Uri {
    /// Parse a string into a `Uri`.
    ///
    /// Returns `None` if the string is not a valid URL.
    pub fn parse(s: &str) -> Option<Self> {
        url::Url::parse(s).ok().map(Uri)
    }

    /// Build a `file://` URI from an absolute filesystem path.
    ///
    /// Percent-encodes the path correctly (spaces, non-ASCII bytes, `#`, `?`,
    /// etc.), unlike `format!("file://{}", path.display())`. Returns `None`
    /// if `path` is not absolute (the only failure mode of
    /// `url::Url::from_file_path`).
    pub fn from_file_path(path: &Path) -> Option<Self> {
        url::Url::from_file_path(path).ok().map(Uri)
    }

    /// The underlying `url::Url`.
    pub fn as_url(&self) -> &url::Url {
        &self.0
    }

    /// The URL scheme (e.g. `file`, `https`, `notion`).
    pub fn scheme(&self) -> &str {
        self.0.scheme()
    }

    /// The raw string representation.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Display with percent-decoded path components for human readability.
    ///
    /// The decoded text is sanitized (see `sanitize_for_display`) — decoding
    /// re-materializes whatever bytes the filename actually held, and those
    /// must never reach a terminal verbatim.
    pub fn display_decoded(&self) -> String {
        let decoded = sanitize_for_display(
            &percent_encoding::percent_decode_str(self.0.path()).decode_utf8_lossy(),
        );

        if let Some(host) = self.0.host_str() {
            format!("{}://{}{}", self.0.scheme(), host, decoded)
        } else {
            format!("{}:{}", self.0.scheme(), decoded)
        }
    }
}

/// Replace characters that must never reach a terminal, log line, or progress
/// bar. A Unix filename may contain any byte but `/` and NUL, so percent-
/// decoding one for display can otherwise emit live ANSI escapes (`%1B`),
/// newlines (`%0A`), or bidi overrides that make `annex\u{202E}dm.exe` render
/// as `annexe.md`. U+FFFD matches what `decode_utf8_lossy` already substitutes
/// for invalid UTF-8, so the output stays internally consistent.
///
/// `char::is_control()` covers C0, C1 and DEL; the bidi ranges are `Cf`, which
/// it does not cover, hence the explicit arms.
fn sanitize_for_display(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            c if c.is_control() => '\u{FFFD}',
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => '\u{FFFD}',
            c => c,
        })
        .collect()
}

/// Percent-decode a URI string for human-readable display.
///
/// `ProgressEvent::DocumentStarted`/`DocumentFinished` carry a raw `String`
/// (always `Uri::as_str()` under the hood, but the event type predates
/// `Uri` and isn't worth widening just for display). Surface crates like
/// `cli` must not re-implement percent-decoding themselves — see
/// `specs/01-architecture.md §1` — so this free function does the decoding
/// on their behalf. Falls back to `raw` if it does not parse as a URI
/// (defensive; every current caller passes an already-valid `Uri::as_str()`).
///
/// Both arms are run through `sanitize_for_display` — the fallback especially,
/// since an unparseable input is exactly the case where nothing else has
/// vetted the bytes.
pub fn display_decoded_uri(raw: &str) -> String {
    Uri::parse(raw)
        .map(|uri| uri.display_decoded())
        .unwrap_or_else(|| sanitize_for_display(raw))
}

impl fmt::Display for Uri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for Uri {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.as_str())
    }
}

impl<'de> Deserialize<'de> for Uri {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        url::Url::parse(&s)
            .map(Uri)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Root of the absolute paths these tests build.
    ///
    /// `Url::from_file_path` — the one failure mode of [`Uri::from_file_path`] — rejects
    /// any path it does not consider absolute, and on Windows that means a path without a
    /// drive prefix. Hardcoding `/home/user` therefore made every `from_file_path` test
    /// assert Unix path syntax rather than the encoding behaviour it was written for.
    #[cfg(unix)]
    const ABS_ROOT: &str = "/home/user";
    #[cfg(windows)]
    const ABS_ROOT: &str = r"C:\home\user";

    /// The `file://` prefix that [`abs`] paths serialize to on this platform.
    #[cfg(unix)]
    const ABS_URI_PREFIX: &str = "file:///home/user/";
    #[cfg(windows)]
    const ABS_URI_PREFIX: &str = "file:///C:/home/user/";

    /// An absolute path to `rel` under [`ABS_ROOT`], for whichever OS is running.
    ///
    /// `rel` is always `/`-separated; joining component by component is what turns it into
    /// native separators, so callers never write a platform's syntax.
    fn abs(rel: &str) -> PathBuf {
        rel.split('/')
            .fold(PathBuf::from(ABS_ROOT), |path, part| path.join(part))
    }

    #[test]
    fn parse_valid_file_uri() {
        let uri = Uri::parse("file:///home/user/docs/test.md").unwrap();
        assert_eq!(uri.scheme(), "file");
        assert_eq!(uri.as_str(), "file:///home/user/docs/test.md");
    }

    #[test]
    fn parse_valid_https_uri() {
        let uri = Uri::parse("https://example.com/page?q=hello").unwrap();
        assert_eq!(uri.scheme(), "https");
    }

    #[test]
    fn parse_preserves_fragment_untouched() {
        // Pinning test for the Atom/RSS feed ingestor's discovery mode
        // (issue #116): link-less entries are addressed by a synthetic
        // fragment URI `{feed_url}#entry:{id}`. `Uri::parse` must round-trip
        // the fragment byte-for-byte, since it's the only thing that makes
        // such an entry's URI unique.
        let uri = Uri::parse("https://example.com/feed.xml#entry:abc123").unwrap();
        assert_eq!(uri.as_str(), "https://example.com/feed.xml#entry:abc123");
        assert_eq!(uri.as_url().fragment(), Some("entry:abc123"));

        // Round trip through Display and re-parse.
        let redisplayed = uri.to_string();
        assert_eq!(redisplayed, "https://example.com/feed.xml#entry:abc123");
        let reparsed = Uri::parse(&redisplayed).unwrap();
        assert_eq!(uri, reparsed);

        // Round trip through serde too.
        let json = serde_json::to_string(&uri).unwrap();
        let from_json: Uri = serde_json::from_str(&json).unwrap();
        assert_eq!(uri, from_json);
    }

    #[test]
    fn parse_connector_scheme() {
        let uri = Uri::parse("notion://page/abc123").unwrap();
        assert_eq!(uri.scheme(), "notion");
    }

    #[test]
    fn rejects_invalid_uri() {
        assert!(Uri::parse("not a url").is_none());
        assert!(Uri::parse("").is_none());
    }

    #[test]
    fn handles_international_chars() {
        let uri = Uri::parse("file:///home/user/%E4%B8%AD%E6%96%87.md").unwrap();
        assert!(uri.as_str().contains("%E4%B8%AD%E6%96%87"));
    }

    #[test]
    fn display_decoded_file() {
        let uri = Uri::parse("file:///home/user/my%20file.md").unwrap();
        let decoded = uri.display_decoded();
        assert!(decoded.contains("my file.md"));
    }

    // The four cases below prove `display_decoded` uses real path
    // percent-decoding, not `url::form_urlencoded::parse` (form-data
    // decoding). `url::Url::from_file_path` — the constructor `FoundFile`
    // actually uses — does NOT percent-encode `&`, `=`, or `+` in a path,
    // since none of them require escaping there per RFC 3986; they pass
    // through literally. That is exactly what breaks the old decoder,
    // which treats the whole path as a `key=value&key=value` form body:
    //
    //   - `&` is treated as a pair separator and silently dropped:
    //     "foo&bar.md" -> "foobar.md".
    //   - `+` is treated as an encoded space: "foo+bar.md" -> "foo bar.md".
    //   - a lone `=` happens to round-trip UNLESS its value half is empty
    //     (i.e. the path ends in `=`), in which case the trailing `=` is
    //     dropped entirely: "notes=" -> "notes".
    //   - a lone non-ASCII sequence also happens to round-trip under the
    //     old code (form_urlencoded percent-decodes UTF-8 correctly too),
    //     so to demonstrate a real failure it must appear alongside a
    //     literal `+`, which still gets corrupted into a space.
    //
    // Each assertion below was verified against the old
    // `form_urlencoded::parse`-based implementation and confirmed to fail;
    // see the commit message for the verbatim before/after output.

    #[test]
    fn display_decoded_ampersand_in_filename() {
        let uri = Uri::from_file_path(&abs("foo&bar.md")).unwrap();
        let decoded = uri.display_decoded();
        // Old: "file:/home/user/foobar.md" (the `&` and everything it was
        // "separating" got silently merged away).
        assert!(
            decoded.contains("foo&bar.md"),
            "expected 'foo&bar.md', got: {decoded}"
        );
    }

    #[test]
    fn display_decoded_trailing_equals_in_filename() {
        let uri = Uri::from_file_path(&abs("notes=")).unwrap();
        let decoded = uri.display_decoded();
        // Old: "file:/home/user/notes" (the trailing `=` vanished because
        // form_urlencoded treats it as a key/value separator with an empty
        // value, and an empty value is dropped by the old `if v.is_empty()`
        // branch).
        assert!(
            decoded.ends_with("notes="),
            "expected trailing '=' to survive, got: {decoded}"
        );
    }

    #[test]
    fn display_decoded_literal_plus_not_turned_into_space() {
        let uri = Uri::from_file_path(&abs("foo+bar.md")).unwrap();
        let decoded = uri.display_decoded();
        // Old: "file:/home/user/foo bar.md" (form_urlencoded decodes a
        // literal `+` byte as an encoded space).
        assert!(
            decoded.contains("foo+bar.md"),
            "expected 'foo+bar.md', got: {decoded}"
        );
    }

    #[test]
    fn display_decoded_non_ascii_with_plus() {
        let uri = Uri::from_file_path(&abs("café+notes.md")).unwrap();
        let decoded = uri.display_decoded();
        // Old: "file:/home/user/café notes.md" — the percent-encoded 'é'
        // decodes fine on its own, but the literal `+` still gets turned
        // into a space, corrupting the non-ASCII filename.
        assert!(
            decoded.contains("café+notes.md"),
            "expected 'café+notes.md', got: {decoded}"
        );
    }

    // Percent-decoding for display re-materializes whatever bytes the
    // filename actually held. A Unix filename may contain any byte but `/`
    // and NUL, so the decoded form can carry live ANSI escapes, newlines, or
    // bidi overrides straight into a terminal, log line, or progress bar.
    // Every such character must come out as U+FFFD.

    #[test]
    fn display_decoded_neutralizes_ansi_escape() {
        let uri = Uri::parse("file:///home/user/evil%1B%5B2J.md").unwrap();
        let decoded = uri.display_decoded();
        assert!(
            !decoded.contains('\u{1B}'),
            "a raw ESC must never survive decoding, got: {decoded:?}"
        );
        assert!(decoded.contains('\u{FFFD}'), "got: {decoded:?}");
    }

    #[test]
    fn display_decoded_neutralizes_newline() {
        let uri = Uri::parse("file:///home/user/a%0Ab.md").unwrap();
        let decoded = uri.display_decoded();
        assert!(!decoded.contains('\n'), "got: {decoded:?}");
        assert!(decoded.contains("a\u{FFFD}b.md"), "got: {decoded:?}");
    }

    #[test]
    fn display_decoded_neutralizes_nul() {
        let uri = Uri::parse("file:///home/user/a%00b.md").unwrap();
        let decoded = uri.display_decoded();
        assert!(!decoded.contains('\0'), "got: {decoded:?}");
        assert!(decoded.contains("a\u{FFFD}b.md"), "got: {decoded:?}");
    }

    #[test]
    fn display_decoded_neutralizes_bidi_override() {
        // U+202E RIGHT-TO-LEFT OVERRIDE makes `annex\u{202E}dm.exe` render
        // as `annexe.md`. It is `Cf`, which `char::is_control` does not
        // cover, hence the explicit range arm in `sanitize_for_display`.
        let uri = Uri::parse("file:///home/user/annex%E2%80%AEdm.exe").unwrap();
        let decoded = uri.display_decoded();
        assert!(!decoded.contains('\u{202E}'), "got: {decoded:?}");
        assert!(decoded.contains("annex\u{FFFD}dm.exe"), "got: {decoded:?}");
    }

    #[test]
    fn display_decoded_leaves_printable_non_ascii_alone() {
        // Guard against over-sanitizing: ordinary international filenames
        // must still render as themselves.
        let uri = Uri::from_file_path(&abs("日本語 café.md")).unwrap();
        let decoded = uri.display_decoded();
        assert!(
            decoded.ends_with("日本語 café.md"),
            "printable non-ASCII must survive, got: {decoded:?}"
        );
        assert!(!decoded.contains('\u{FFFD}'), "got: {decoded:?}");
    }

    #[test]
    fn display_decoded_uri_str_sanitizes_the_unparseable_fallback() {
        // An input that does not parse as a URI is exactly the case where
        // nothing else has vetted the bytes, so the fallback arm must be
        // sanitized too.
        let raw = "not a uri \u{1B}[2J at all";
        let out = display_decoded_uri(raw);
        assert!(!out.contains('\u{1B}'), "got: {out:?}");
        assert_eq!(out, "not a uri \u{FFFD}[2J at all");
    }

    #[test]
    fn display_decoded_uri_str_decodes_a_valid_uri_string() {
        let raw = "file:///home/user/my%20file.md";
        assert_eq!(display_decoded_uri(raw), "file:/home/user/my file.md");
    }

    #[test]
    fn display_decoded_uri_str_falls_back_on_unparseable_input() {
        let raw = "not a uri at all";
        assert_eq!(display_decoded_uri(raw), raw);
    }

    #[test]
    fn serde_roundtrip() {
        let uri = Uri::parse("https://example.com/path").unwrap();
        let json = serde_json::to_string(&uri).unwrap();
        let deserialized: Uri = serde_json::from_str(&json).unwrap();
        assert_eq!(uri, deserialized);
    }

    #[test]
    fn serde_rejects_invalid() {
        let result: Result<Uri, _> = serde_json::from_str("\"not a url\"");
        assert!(result.is_err());
    }

    #[test]
    fn from_file_path_builds_file_uri() {
        let uri = Uri::from_file_path(&abs("docs/test.md")).unwrap();
        assert_eq!(uri.scheme(), "file");
        assert_eq!(uri.as_str(), format!("{ABS_URI_PREFIX}docs/test.md"));
    }

    #[test]
    fn from_file_path_rejects_relative_path() {
        assert!(Uri::from_file_path(Path::new("relative/path.md")).is_none());
    }

    #[test]
    fn from_file_path_encodes_space() {
        let uri = Uri::from_file_path(&abs("my file.md")).unwrap();
        assert_eq!(uri.as_str(), format!("{ABS_URI_PREFIX}my%20file.md"));
    }

    #[test]
    fn from_file_path_encodes_non_ascii() {
        let uri = Uri::from_file_path(&abs("中文.md")).unwrap();
        assert!(uri.as_str().contains("%E4%B8%AD%E6%96%87"));
    }

    #[test]
    fn equality_and_hash() {
        let a = Uri::parse("file:///test.md").unwrap();
        let b = Uri::parse("file:///test.md").unwrap();
        assert_eq!(a, b);

        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
    }
}
