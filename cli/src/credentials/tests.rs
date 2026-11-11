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

// Env-key precedence, the credentials-file fallback, and the "nothing
// available" case are pinned at the `daemon_client::fresh_bearer_for_url`
// level (`daemon_client::tests::auth`), the one place that now resolves a
// request's bearer — this module's tests stick to the lookup/write
// primitives it builds on.

// -----------------------------------------------------------------
// Mutation paths must never treat an unreadable/malformed file as empty:
// only a missing file is empty; anything else is a reported error so a
// write/remove can't silently replace credentials it failed to parse.
// -----------------------------------------------------------------

#[test]
fn write_entry_fails_on_malformed_json_instead_of_overwriting() {
    let dir = TempDir::new().unwrap();
    let path = write_credentials(&dir, "not json at all");

    let result = write_entry(
        &path,
        "http://127.0.0.1:7700",
        CredentialEntry {
            secret: Some("ldb_new".into()),
            ..Default::default()
        },
    );

    assert!(result.is_err());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "not json at all",
        "a malformed file must be left untouched, not replaced with the new entry alone"
    );
}

#[test]
fn remove_entry_fails_on_malformed_json_instead_of_overwriting() {
    let dir = TempDir::new().unwrap();
    let path = write_credentials(&dir, "not json at all");

    let result = remove_entry(&path, "http://127.0.0.1:7700");

    assert!(result.is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json at all");
}

#[test]
fn write_entry_error_names_the_file() {
    let dir = TempDir::new().unwrap();
    let path = write_credentials(&dir, "not json at all");

    let error = write_entry(&path, "http://x", CredentialEntry::default()).unwrap_err();

    assert!(
        error.to_string().contains(&path.display().to_string()),
        "error should name the unparseable file: {error}"
    );
}

#[test]
fn write_entry_on_unreadable_directory_path_fails_rather_than_overwriting() {
    // A credentials_file path that is itself a directory can't be read as a
    // file; this must surface as an error, not be treated as "missing".
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("credentials.json");
    std::fs::create_dir(&path).unwrap();

    let result = write_entry(&path, "http://x", CredentialEntry::default());
    assert!(result.is_err());
}

// -----------------------------------------------------------------
// Temp-file handling: a failed write must never leave the predictable
// final path, or any temp file, behind.
// -----------------------------------------------------------------

#[test]
fn write_file_removes_temp_file_when_rename_fails() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("credentials.json");
    // Occupy the destination with a directory so the final rename fails
    // (a regular file can never be renamed onto an existing directory).
    std::fs::create_dir(&path).unwrap();

    let result = write_file(&path, &CredentialsFile::default());

    assert!(result.is_err());
    let leftover: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
        .collect();
    assert!(
        leftover.is_empty(),
        "a failed rename must not leave a temp file behind: {leftover:?}"
    );
}

// -----------------------------------------------------------------
// Writer, login-token shape, atomicity, permissions
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
    // Backward compatibility: an entry with only `secret` (the legacy shape,
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

// -----------------------------------------------------------------
// Path-prefixed daemon credentials: a stored key is the longest
// path-segment prefix of the request URL, not just its origin.
// -----------------------------------------------------------------

#[test]
fn lookup_entry_for_url_matches_a_path_prefixed_daemon() {
    let dir = TempDir::new().unwrap();
    let path = write_credentials(
        &dir,
        r#"{"version":1,"credentials":{"https://example.com/team":{"secret":"ldb_team"}}}"#,
    );
    let (key, entry) = lookup_entry_for_url(&path, "https://example.com/team/v1/stores").unwrap();
    assert_eq!(key, "https://example.com/team");
    assert_eq!(entry.secret.as_deref(), Some("ldb_team"));
}

#[test]
fn lookup_entry_for_url_requires_a_path_segment_boundary() {
    let dir = TempDir::new().unwrap();
    let path = write_credentials(
        &dir,
        r#"{"version":1,"credentials":{"https://example.com/team":{"secret":"ldb_team"}}}"#,
    );
    assert!(
        lookup_entry_for_url(&path, "https://example.com/teamX/v1").is_none(),
        "a sibling path that merely shares a string prefix must not match"
    );
}

#[test]
fn lookup_entry_for_url_prefers_the_longest_matching_prefix() {
    let dir = TempDir::new().unwrap();
    let path = write_credentials(
        &dir,
        concat!(
            r#"{"version":1,"credentials":{"#,
            r#""https://example.com":{"secret":"ldb_origin"},"#,
            r#""https://example.com/team":{"secret":"ldb_team"}"#,
            r#"}}"#
        ),
    );
    let (key, entry) = lookup_entry_for_url(&path, "https://example.com/team/v1/stores").unwrap();
    assert_eq!(key, "https://example.com/team");
    assert_eq!(entry.secret.as_deref(), Some("ldb_team"));
}

#[test]
fn lookup_entry_for_url_trailing_slash_on_stored_key_is_equivalent() {
    let dir = TempDir::new().unwrap();
    let path = write_credentials(
        &dir,
        r#"{"version":1,"credentials":{"https://example.com/team/":{"secret":"ldb_team"}}}"#,
    );
    let (key, entry) = lookup_entry_for_url(&path, "https://example.com/team/v1/stores").unwrap();
    assert_eq!(
        key, "https://example.com/team",
        "the matched key is always returned in its canonical (no trailing slash) spelling"
    );
    assert_eq!(entry.secret.as_deref(), Some("ldb_team"));
}

#[test]
fn lookup_entry_for_url_matched_key_round_trips_through_lookup_entry() {
    // `refresh_cached_bearer` passes the key `lookup_entry_for_url` returns
    // straight into an exact `lookup_entry`/`CredentialLock::lookup` call, so
    // the two must agree on spelling even when the file only ever stored the
    // legacy trailing-slash form.
    let dir = TempDir::new().unwrap();
    let path = write_credentials(
        &dir,
        r#"{"version":1,"credentials":{"https://example.com/team/":{"secret":"ldb_team"}}}"#,
    );
    let (key, _) = lookup_entry_for_url(&path, "https://example.com/team/v1/stores").unwrap();
    assert!(
        lookup_entry(&path, &key).is_some(),
        "the key returned by lookup_entry_for_url must be found by an exact lookup_entry call"
    );
}

#[test]
fn lookup_entry_for_url_prefers_canonical_entry_when_both_spellings_stored() {
    let dir = TempDir::new().unwrap();
    let path = write_credentials(
        &dir,
        concat!(
            r#"{"version":1,"credentials":{"#,
            r#""https://example.com/team":{"secret":"ldb_canonical"},"#,
            r#""https://example.com/team/":{"secret":"ldb_slash"}"#,
            r#"}}"#
        ),
    );
    let (_, entry) = lookup_entry_for_url(&path, "https://example.com/team/v1/stores").unwrap();
    assert_eq!(
        entry.secret.as_deref(),
        Some("ldb_canonical"),
        "when both spellings of a key are stored, the already-canonical entry wins deterministically"
    );
}

#[test]
fn write_entry_canonicalizes_a_pre_existing_legacy_trailing_slash_key() {
    let dir = TempDir::new().unwrap();
    let path = write_credentials(
        &dir,
        r#"{"version":1,"credentials":{"https://example.com/team/":{"secret":"ldb_team"}}}"#,
    );
    write_entry(
        &path,
        "https://other.example",
        CredentialEntry {
            secret: Some("ldb_other".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(
        lookup_secret(&path, "https://example.com/team").as_deref(),
        Some("ldb_team"),
        "an unrelated write canonicalizes every stored key, including one it didn't touch"
    );
}

#[test]
fn lookup_entry_for_url_origin_only_entry_still_matches_origin_requests() {
    let dir = TempDir::new().unwrap();
    let path = write_credentials(
        &dir,
        r#"{"version":1,"credentials":{"http://127.0.0.1:7700":{"secret":"ldb_cached"}}}"#,
    );
    let (key, entry) = lookup_entry_for_url(&path, "http://127.0.0.1:7700/v1/status").unwrap();
    assert_eq!(key, "http://127.0.0.1:7700");
    assert_eq!(entry.secret.as_deref(), Some("ldb_cached"));
}

#[test]
fn lookup_entry_for_url_matches_a_path_prefixed_daemon_request() {
    let dir = TempDir::new().unwrap();
    let path = write_credentials(
        &dir,
        r#"{"version":1,"credentials":{"https://example.com/team":{"secret":"ldb_team"}}}"#,
    );
    let (_, entry) = lookup_entry_for_url(&path, "https://example.com/team/v1/stores").unwrap();
    assert_eq!(entry.secret.as_deref(), Some("ldb_team"));
}

#[tokio::test]
async fn locked_mutations_preserve_concurrent_origins_and_release_on_drop() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("credentials.json");
    let held = CredentialLock::acquire(&path).await.unwrap();
    let other_path = path.clone();
    let waiting = tokio::spawn(async move {
        let lock = CredentialLock::acquire(&other_path).await.unwrap();
        lock.write(
            "https://second.example",
            CredentialEntry {
                secret: Some("second".into()),
                ..Default::default()
            },
        )
        .unwrap();
    });
    held.write(
        "https://first.example",
        CredentialEntry {
            secret: Some("first".into()),
            ..Default::default()
        },
    )
    .unwrap();
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    drop(held);
    tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        lookup_secret(&path, "https://first.example").as_deref(),
        Some("first")
    );
    assert_eq!(
        lookup_secret(&path, "https://second.example").as_deref(),
        Some("second")
    );
}
