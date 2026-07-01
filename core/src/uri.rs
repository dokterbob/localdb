use std::fmt;

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
    pub fn display_decoded(&self) -> String {
        let decoded = url::form_urlencoded::parse(self.0.path().as_bytes())
            .map(|(k, v)| {
                if v.is_empty() {
                    k.into_owned()
                } else {
                    format!("{k}={v}")
                }
            })
            .collect::<Vec<_>>()
            .join("");

        if let Some(host) = self.0.host_str() {
            format!("{}://{}{}", self.0.scheme(), host, decoded)
        } else {
            format!("{}:{}", self.0.scheme(), decoded)
        }
    }
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
