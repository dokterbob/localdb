//! Path-source tests: `normalize_path_source` and the `"path"` arm of
//! `parse_source_spec`.

use crate::source::kinds::path::normalize_path_source;
use crate::source::kinds::tests::common::{
    default_path_excludes, default_path_includes, invalid_request,
};
use crate::source::{parse_source_spec, ParsedSourceSpec};
use crate::types::SourceKind;

#[test]
fn normalize_path_source_returns_file_parent_and_filename_when_path_is_file() {
    // Given
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("note.md");
    std::fs::write(&file_path, "hello").unwrap();

    // When
    let (root, include, exclude) = normalize_path_source(&file_path.to_string_lossy()).unwrap();

    // Then
    assert_eq!(root, temp_dir.path().to_string_lossy());
    assert_eq!(include, vec!["note.md".to_string()]);
    assert_eq!(exclude, default_path_excludes());
}

#[test]
fn normalize_path_source_returns_error_when_path_is_missing() {
    // Given
    let temp_dir = tempfile::tempdir().unwrap();
    let missing_path = temp_dir.path().join("missing.md");

    // When
    let err = normalize_path_source(&missing_path.to_string_lossy()).unwrap_err();

    // Then
    assert_eq!(
        err,
        invalid_request(&format!(
            "path '{}' does not exist",
            missing_path.to_string_lossy()
        ))
    );
}

#[test]
fn normalize_path_source_returns_directory_defaults_when_path_is_directory() {
    // Given
    let temp_dir = tempfile::tempdir().unwrap();

    // When
    let (root, include, exclude) =
        normalize_path_source(&temp_dir.path().to_string_lossy()).unwrap();

    // Then
    assert_eq!(root, temp_dir.path().to_string_lossy());
    assert_eq!(include, default_path_includes());
    assert_eq!(exclude, default_path_excludes());
}

#[test]
fn parse_source_spec_returns_path_fields_when_path_spec_is_valid() {
    // Given
    let spec = serde_json::json!({
        "root": "/tmp/docs",
        "include": ["**/*.md"],
        "exclude": ["**/.git"],
    });

    // When
    let parsed = parse_source_spec("path", &spec).unwrap();

    // Then
    assert_eq!(
        parsed,
        ParsedSourceSpec {
            kind: SourceKind::Path,
            root: Some("/tmp/docs".to_string()),
            url: None,
            include: vec!["**/*.md".to_string()],
            exclude: vec!["**/.git".to_string()],
            config_json: None,
        }
    );
}

#[test]
fn parse_source_spec_returns_error_when_array_field_contains_non_string() {
    // Given
    let spec = serde_json::json!({"root": "/tmp/docs", "include": [42]});

    // When
    let err = parse_source_spec("path", &spec).unwrap_err();

    // Then
    assert_eq!(
        err,
        invalid_request("source spec field 'include' contains a non-string value")
    );
}
