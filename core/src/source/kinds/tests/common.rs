//! Shared test helpers for source-kind tests.

use crate::error::Error;
use crate::source::kinds::path::{DEFAULT_PATH_EXCLUDES, DEFAULT_PATH_INCLUDES};

pub(in crate::source) fn default_path_includes() -> Vec<String> {
    DEFAULT_PATH_INCLUDES
        .iter()
        .map(|value| value.to_string())
        .collect()
}

pub(in crate::source) fn default_path_excludes() -> Vec<String> {
    DEFAULT_PATH_EXCLUDES
        .iter()
        .map(|value| value.to_string())
        .collect()
}

pub(in crate::source) fn invalid_request(message: &str) -> Error {
    Error::InvalidRequest {
        message: message.to_string(),
    }
}
