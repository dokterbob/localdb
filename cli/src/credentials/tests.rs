use super::*;
use tempfile::TempDir;

fn write_credentials(dir: &TempDir, body: &str) -> PathBuf {
    let path = dir.path().join("credentials.json");
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn credentials_path_is_sibling_of_config() {
    assert_eq!(
        credentials_path(Path::new("/etc/localdb/config.yaml")),
        PathBuf::from("/etc/localdb/credentials.json")
    );
}

#[test]
fn lookup_finds_secret_by_base_url() {
    let dir = TempDir::new().unwrap();
    let path = write_credentials(
        &dir,
        r#"{"version":1,"credentials":{"http://127.0.0.1:7700":{"secret":"ldb_cached"}}}"#,
    );
    assert_eq!(
        lookup_secret(&path, "http://127.0.0.1:7700").as_deref(),
        Some("ldb_cached")
    );
}

#[test]
fn lookup_normalizes_trailing_slash() {
    let dir = TempDir::new().unwrap();
    let path = write_credentials(
        &dir,
        r#"{"version":1,"credentials":{"http://127.0.0.1:7700":{"secret":"ldb_cached"}}}"#,
    );
    assert_eq!(
        lookup_secret(&path, "http://127.0.0.1:7700/").as_deref(),
        Some("ldb_cached")
    );
}

#[test]
fn lookup_returns_none_for_unknown_url() {
    let dir = TempDir::new().unwrap();
    let path = write_credentials(
        &dir,
        r#"{"version":1,"credentials":{"http://127.0.0.1:7700":{"secret":"ldb_cached"}}}"#,
    );
    assert_eq!(lookup_secret(&path, "http://10.0.0.5:7700"), None);
}

#[test]
fn lookup_returns_none_for_missing_file() {
    let dir = TempDir::new().unwrap();
    assert_eq!(
        lookup_secret(&dir.path().join("credentials.json"), "http://x"),
        None
    );
}

#[test]
fn lookup_returns_none_for_malformed_json() {
    let dir = TempDir::new().unwrap();
    let path = write_credentials(&dir, "not json at all");
    assert_eq!(lookup_secret(&path, "http://127.0.0.1:7700"), None);
}

#[test]
fn resolve_bearer_prefers_env_api_key() {
    let dir = TempDir::new().unwrap();
    write_credentials(
        &dir,
        r#"{"version":1,"credentials":{"http://127.0.0.1:7700":{"secret":"ldb_cached"}}}"#,
    );
    let config_file = dir.path().join("config.yaml");
    let resolved = resolve_bearer(
        Some("ldb_from_env"),
        Some(&config_file),
        "http://127.0.0.1:7700",
    );
    assert_eq!(resolved.as_deref(), Some("ldb_from_env"));
}

#[test]
fn resolve_bearer_falls_back_to_credentials_file() {
    let dir = TempDir::new().unwrap();
    write_credentials(
        &dir,
        r#"{"version":1,"credentials":{"http://127.0.0.1:7700":{"secret":"ldb_cached"}}}"#,
    );
    let config_file = dir.path().join("config.yaml");
    let resolved = resolve_bearer(None, Some(&config_file), "http://127.0.0.1:7700");
    assert_eq!(resolved.as_deref(), Some("ldb_cached"));
}

#[test]
fn resolve_bearer_empty_env_key_is_ignored() {
    let dir = TempDir::new().unwrap();
    write_credentials(
        &dir,
        r#"{"version":1,"credentials":{"http://127.0.0.1:7700":{"secret":"ldb_cached"}}}"#,
    );
    let config_file = dir.path().join("config.yaml");
    let resolved = resolve_bearer(Some(""), Some(&config_file), "http://127.0.0.1:7700");
    assert_eq!(resolved.as_deref(), Some("ldb_cached"));
}

#[test]
fn resolve_bearer_none_when_nothing_available() {
    let dir = TempDir::new().unwrap();
    let config_file = dir.path().join("config.yaml");
    assert_eq!(
        resolve_bearer(None, Some(&config_file), "http://127.0.0.1:7700"),
        None
    );
    assert_eq!(resolve_bearer(None, None, "http://127.0.0.1:7700"), None);
}

// -----------------------------------------------------------------
// T4: writer, login-token shape, atomicity, permissions
// -----------------------------------------------------------------

#[test]
fn write_entry_then_lookup_round_trips_access_token() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("credentials.json");
    let entry = CredentialEntry {
        secret: None,
        access_token: Some("ldb_access".to_string()),
        refresh_token: Some("ldb_refresh".to_string()),
        access_expires_at: Some("2026-07-07T13:00:00Z".to_string()),
    };
    write_entry(&path, "http://127.0.0.1:7700", entry.clone()).unwrap();

    let found = lookup_entry(&path, "http://127.0.0.1:7700").unwrap();
    assert_eq!(found, entry);
    assert_eq!(
        lookup_secret(&path, "http://127.0.0.1:7700").as_deref(),
        Some("ldb_access"),
        "access_token is preferred over secret when both could apply"
    );
}

#[test]
fn write_entry_preserves_other_base_urls() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("credentials.json");
    write_entry(
        &path,
        "http://127.0.0.1:7700",
        CredentialEntry {
            secret: Some("ldb_one".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    write_entry(
        &path,
        "http://127.0.0.1:7701",
        CredentialEntry {
            secret: Some("ldb_two".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(
        lookup_secret(&path, "http://127.0.0.1:7700").as_deref(),
        Some("ldb_one")
    );
    assert_eq!(
        lookup_secret(&path, "http://127.0.0.1:7701").as_deref(),
        Some("ldb_two")
    );
}

#[test]
fn write_entry_overwrites_same_base_url() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("credentials.json");
    write_entry(
        &path,
        "http://127.0.0.1:7700",
        CredentialEntry {
            secret: Some("ldb_old".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    write_entry(
        &path,
        "http://127.0.0.1:7700",
        CredentialEntry {
            access_token: Some("ldb_new".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(
        lookup_secret(&path, "http://127.0.0.1:7700").as_deref(),
        Some("ldb_new")
    );
}

#[test]
fn write_entry_sets_0600_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("credentials.json");
    write_entry(
        &path,
        "http://127.0.0.1:7700",
        CredentialEntry {
            secret: Some("ldb_secret".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "credentials.json must be 0600, got {mode:o}");
}

#[test]
fn write_entry_leaves_no_leftover_tmp_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("credentials.json");
    write_entry(
        &path,
        "http://127.0.0.1:7700",
        CredentialEntry {
            secret: Some("ldb_secret".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    let leftover: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
        .collect();
    assert!(
        leftover.is_empty(),
        "no temp file should remain after an atomic write: {leftover:?}"
    );
    assert!(path.exists(), "the final credentials.json must exist");
}

#[test]
fn remove_entry_removes_only_the_named_base_url() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("credentials.json");
    write_entry(
        &path,
        "http://127.0.0.1:7700",
        CredentialEntry {
            secret: Some("ldb_one".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    write_entry(
        &path,
        "http://127.0.0.1:7701",
        CredentialEntry {
            secret: Some("ldb_two".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    let removed = remove_entry(&path, "http://127.0.0.1:7700").unwrap();
    assert!(removed);
    assert!(lookup_entry(&path, "http://127.0.0.1:7700").is_none());
    assert!(lookup_entry(&path, "http://127.0.0.1:7701").is_some());
}

#[test]
fn remove_entry_missing_returns_false() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("credentials.json");
    assert!(!remove_entry(&path, "http://127.0.0.1:7700").unwrap());
}

#[test]
fn legacy_secret_only_file_still_reads_via_lookup_entry() {
    // Backward compatibility: an entry with only `secret` (pre-T4 shape,
    // or an API key a human pasted in by hand) must still round-trip
    // through the richer `CredentialEntry` struct.
    let dir = TempDir::new().unwrap();
    let path = write_credentials(
        &dir,
        r#"{"version":1,"credentials":{"http://127.0.0.1:7700":{"secret":"ldb_legacy"}}}"#,
    );
    let entry = lookup_entry(&path, "http://127.0.0.1:7700").unwrap();
    assert_eq!(entry.secret.as_deref(), Some("ldb_legacy"));
    assert!(entry.access_token.is_none());
}
