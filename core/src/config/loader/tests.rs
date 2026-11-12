use super::*;

// --- parse_duration tests ---

#[test]
fn parse_duration_hours() {
    assert_eq!(parse_duration("24h").unwrap(), 24 * 3600);
    assert_eq!(parse_duration("1h").unwrap(), 3600);
}

#[test]
fn parse_duration_minutes() {
    assert_eq!(parse_duration("30m").unwrap(), 30 * 60);
    assert_eq!(parse_duration("1m").unwrap(), 60);
}

#[test]
fn parse_duration_seconds() {
    assert_eq!(parse_duration("90s").unwrap(), 90);
    assert_eq!(parse_duration("1s").unwrap(), 1);
}

#[test]
fn parse_duration_days() {
    assert_eq!(parse_duration("7d").unwrap(), 7 * 86400);
}

#[test]
fn parse_duration_rejects_invalid() {
    assert!(parse_duration("not-a-duration").is_err());
    assert!(parse_duration("1x").is_err());
    assert!(parse_duration("").is_err());
    assert!(parse_duration("abc").is_err());
}

#[test]
fn parse_duration_rejects_zero() {
    assert!(parse_duration("0h").is_err());
    assert!(parse_duration("0m").is_err());
}

// --- load_config_from_str tests ---

#[test]
fn load_valid_minimal_config() {
    let yaml = "version: 1\n";
    let cfg = load_config_from_str(yaml).expect("valid minimal config should load");
    assert_eq!(cfg.version, 1);
}

#[test]
fn load_rejects_missing_version() {
    // No version field → serde error (missing required field)
    let yaml = "server:\n  bind: 127.0.0.1\n";
    let err = load_config_from_str(yaml).unwrap_err();
    assert!(matches!(err, Error::InvalidConfig { .. }));
}

#[test]
fn load_rejects_unknown_version() {
    let yaml = "version: 99\n";
    let err = load_config_from_str(yaml).unwrap_err();
    match err {
        Error::InvalidConfig { message } => {
            assert!(
                message.contains("99"),
                "error should mention the version number"
            );
            assert!(
                message.contains("version 1"),
                "error should mention supported version"
            );
        }
        other => panic!("expected InvalidConfig, got {:?}", other),
    }
}

#[test]
fn load_rejects_unversioned_with_hint() {
    // Missing version field — error must contain a hint per spec §5.
    let yaml = "server:\n  bind: 127.0.0.1\n  port: 7700\n";
    let err = load_config_from_str(yaml).unwrap_err();
    match err {
        Error::InvalidConfig { message } => {
            assert!(
                message.contains("version: 1") || message.contains("version"),
                "error for unversioned config should contain a hint, got: {}",
                message
            );
        }
        other => panic!("expected InvalidConfig, got {:?}", other),
    }
}

#[test]
fn load_rejects_typo_key() {
    let yaml = "version: 1\nservre:\n  bind: 127.0.0.1\n";
    let err = load_config_from_str(yaml).unwrap_err();
    match err {
        Error::InvalidConfig { message } => {
            // Error should mention the unknown key
            assert!(
                message.contains("servre") || message.contains("unknown"),
                "error message '{}' should mention the typo'd key",
                message
            );
        }
        other => panic!("expected InvalidConfig, got {:?}", other),
    }
}

#[test]
fn load_rejects_typo_in_defaults_chunking() {
    let yaml = r#"
version: 1
defaults:
  indexing:
    chunkng:
      preset_overrides: {}
    embedding:
      model: pplx-embed-context-v1-0.6b
      provider: local-onnx
"#;
    let err = load_config_from_str(yaml).unwrap_err();
    assert!(
        matches!(err, Error::InvalidConfig { .. }),
        "typo in defaults.indexing should fail: {:?}",
        err
    );
}

#[test]
fn config_with_stores_key_is_rejected() {
    // stores: is no longer a valid config key — DB is the single source of truth.
    let yaml = "version: 1\nstores:\n  - name: notes\n";
    let err = load_config_from_str(yaml).unwrap_err();
    assert!(
        matches!(err, Error::InvalidConfig { .. }),
        "stores: key should be rejected by deny_unknown_fields: {:?}",
        err
    );
}

// --- server.job_workers validation ---

#[test]
fn server_job_workers_absent_defaults_to_one() {
    let cfg = load_config_from_str("version: 1\n").expect("minimal config should load");
    assert_eq!(cfg.server.job_workers, 1);
}

#[test]
fn server_job_workers_set_is_respected() {
    let yaml = "version: 1\nserver:\n  job_workers: 4\n";
    let cfg = load_config_from_str(yaml).expect("valid server.job_workers should load");
    assert_eq!(cfg.server.job_workers, 4);
}

#[test]
fn server_job_workers_zero_rejected() {
    let yaml = "version: 1\nserver:\n  job_workers: 0\n";
    let err = load_config_from_str(yaml).unwrap_err();
    match err {
        Error::InvalidConfig { message } => {
            assert!(
                message.contains("server.job_workers"),
                "error message '{}' should mention the offending path",
                message
            );
        }
        other => panic!("expected InvalidConfig, got {:?}", other),
    }
}

// --- http.rate_limit validation ---

#[test]
fn http_rate_limit_requests_per_second_zero_rejected() {
    let yaml = "version: 1\nhttp:\n  rate_limit:\n    requests_per_second: 0\n";
    let err = load_config_from_str(yaml).unwrap_err();
    match err {
        Error::InvalidConfig { message } => {
            assert!(
                message.contains("http.rate_limit.requests_per_second"),
                "error message '{}' should mention the offending path",
                message
            );
        }
        other => panic!("expected InvalidConfig, got {:?}", other),
    }
}

#[test]
fn http_rate_limit_burst_zero_rejected() {
    let yaml = "version: 1\nhttp:\n  rate_limit:\n    burst: 0\n";
    let err = load_config_from_str(yaml).unwrap_err();
    match err {
        Error::InvalidConfig { message } => {
            assert!(
                message.contains("http.rate_limit.burst"),
                "error message '{}' should mention the offending path",
                message
            );
        }
        other => panic!("expected InvalidConfig, got {:?}", other),
    }
}

#[test]
fn http_rate_limit_negative_requests_per_second_rejected_at_deserialize_time() {
    // requests_per_second is u32, so a negative literal fails serde_yaml
    // deserialization before validate_config ever runs — still surfaces
    // as InvalidConfig, just from the parse arm rather than the
    // validation arm.
    let yaml = "version: 1\nhttp:\n  rate_limit:\n    requests_per_second: -1\n";
    let err = load_config_from_str(yaml).unwrap_err();
    assert!(
        matches!(err, Error::InvalidConfig { .. }),
        "negative requests_per_second should fail to deserialize as u32: {:?}",
        err
    );
}

// --- http.user_agent validation ---

/// A control character in `user_agent` makes every
/// `reqwest::ClientBuilder::build()` fail, which would abort an index job
/// — even one that never fetches a URL — with an opaque client-build
/// error. It has to be rejected at load time, naming the key.
#[test]
fn load_rejects_control_character_in_user_agent() {
    let yaml = "version: 1\nhttp:\n  user_agent: \"bad\\nagent\"\n";
    let err = load_config_from_str(yaml).unwrap_err();
    assert_eq!(err.code(), "invalid_config", "got {err:?}");
    match err {
        Error::InvalidConfig { message } => {
            assert!(
                message.contains("http.user_agent"),
                "error message '{}' should mention the offending path",
                message
            );
        }
        other => panic!("expected InvalidConfig, got {:?}", other),
    }
}

/// The rule is `HeaderValue`'s, not a hand-rolled ASCII check: an ordinary
/// UA string passes, and so does one carrying obs-text (0x80-0xFF), which
/// an `is_ascii_graphic` predicate would wrongly reject.
#[test]
fn load_accepts_valid_user_agent_including_obs_text() {
    for ua in ["localdb/9.9 (+https://example.test)", "café-agent/1.0"] {
        let yaml = format!("version: 1\nhttp:\n  user_agent: \"{ua}\"\n");
        let cfg = load_config_from_str(&yaml)
            .unwrap_or_else(|e| panic!("{ua:?} should be a valid header value: {e:?}"));
        assert_eq!(cfg.http.user_agent.as_deref(), Some(ua));
    }
}

#[test]
fn http_rate_limit_valid_values_accepted() {
    let yaml = "version: 1\nhttp:\n  rate_limit:\n    requests_per_second: 2\n    burst: 8\n";
    let cfg = load_config_from_str(yaml).expect("valid http.rate_limit should load");
    assert_eq!(cfg.http.rate_limit.requests_per_second, 2);
    assert_eq!(cfg.http.rate_limit.burst, 8);
}

#[test]
fn load_valid_full_config() {
    let yaml = r#"
version: 1

server:
  bind: 127.0.0.1
  port: 7700

paths:
  data: ~
  models: ~
  logs: ~

defaults:
  indexing:
    chunking:
      preset_overrides: {}
    embedding:
      model: pplx-embed-context-v1-0.6b
      provider: local-onnx

providers:
  - name: my-ollama
    kind: openai-compatible
    base_url: http://localhost:11434/v1
    api_key_env: OLLAMA_KEY
"#;
    let cfg = load_config_from_str(yaml).expect("valid full config should load");
    assert_eq!(cfg.version, 1);
    assert_eq!(cfg.providers.len(), 1);
    assert_eq!(cfg.providers[0].name, "my-ollama");
}

// --- fixture file tests ---

#[test]
fn fixture_valid_loads_successfully() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/config/valid.yaml"
    );
    let yaml = std::fs::read_to_string(path).expect("fixture file should exist");
    let cfg = load_config_from_str(&yaml).expect("valid fixture should load");
    assert_eq!(cfg.version, 1);
}

#[test]
fn fixture_typo_key_rejected() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/config/typo_key.yaml"
    );
    let yaml = std::fs::read_to_string(path).expect("fixture file should exist");
    let err = load_config_from_str(&yaml).unwrap_err();
    assert!(
        matches!(err, Error::InvalidConfig { .. }),
        "typo'd key fixture should fail: {:?}",
        err
    );
}

#[test]
fn fixture_bad_duration_rejected() {
    // Fixture now tests that `stores:` key is rejected (unknown field).
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/config/bad_duration.yaml"
    );
    let yaml = std::fs::read_to_string(path).expect("fixture file should exist");
    let err = load_config_from_str(&yaml).unwrap_err();
    assert!(
        matches!(err, Error::InvalidConfig { .. }),
        "bad_duration fixture should fail: {:?}",
        err
    );
}

#[test]
fn fixture_unversioned_rejected_with_hint() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/config/unversioned.yaml"
    );
    let yaml = std::fs::read_to_string(path).expect("fixture file should exist");
    let err = load_config_from_str(&yaml).unwrap_err();
    match err {
        Error::InvalidConfig { message } => {
            assert!(
                message.contains("version: 1") || message.contains("version"),
                "unversioned fixture error should contain a hint, got: {}",
                message
            );
        }
        other => panic!("expected InvalidConfig, got {:?}", other),
    }
}

// --- load_config file-path override tests ---

#[test]
fn load_config_with_explicit_path_option() {
    // LoadOptions.config_path overrides env and platform default.
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/config/valid.yaml"
    );
    let options = LoadOptions {
        config_path: Some(std::path::PathBuf::from(path)),
        ..Default::default()
    };
    let loader =
        load_config(&options, None).expect("load_config with explicit path should succeed");
    assert_eq!(loader.config.version, 1);
    assert_eq!(loader.paths.config_file, std::path::PathBuf::from(path));
}

#[test]
fn load_config_env_var_override() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/config/valid.yaml"
    );
    let loader = load_config(&LoadOptions::default(), Some(Path::new(path)))
        .expect("load_config via env config path should succeed");
    assert_eq!(loader.config.version, 1);
}

// --- Tilde expansion test ---

#[test]
fn expand_path_expands_tilde() {
    // A path starting with `~/` should have `~` replaced by the home directory.
    if let Some(home) = dirs::home_dir() {
        let expanded = expand_path(&"~/Documents/notes".to_string());
        assert!(
            expanded.starts_with(&home),
            "expanded path {:?} should start with home dir {:?}",
            expanded,
            home
        );
        assert!(
            expanded.ends_with("Documents/notes"),
            "expanded path {:?} should end with Documents/notes",
            expanded
        );
    }
}

#[test]
fn expand_path_passes_through_absolute_path() {
    let p = expand_path(&"/absolute/path".to_string());
    assert_eq!(p, std::path::PathBuf::from("/absolute/path"));
}

// --- YAML file bytes never written test (enforced at type level here) ---

#[test]
fn config_yaml_file_not_written_after_load() {
    // Load a fixture, then verify its bytes on disk are unchanged.
    // Structural invariant: RawConfig and ConfigLoader expose no write-to-file methods.
    let path_str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/config/valid.yaml"
    );
    let before = std::fs::read(path_str).expect("fixture must exist");
    let options = LoadOptions {
        config_path: Some(std::path::PathBuf::from(path_str)),
        ..Default::default()
    };
    let _loader = load_config(&options, None).expect("should load");
    let after = std::fs::read(path_str).expect("fixture must still exist");
    assert_eq!(
        before, after,
        "config file must not be modified by load_config"
    );
}

#[test]
fn refuses_to_open_with_legacy_runtime_state_db() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("runtime-state.db"), b"legacy").unwrap();
    let result = refuse_legacy_layout(dir.path());
    match result {
        Err(Error::InvalidConfig { message }) => {
            assert!(message.contains("legacy") || message.contains("runtime-state.db"));
        }
        other => panic!("expected InvalidConfig, got: {other:?}"),
    }
}

#[test]
fn refuses_to_open_with_legacy_stores_dir() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("stores").join("notes")).unwrap();
    let result = refuse_legacy_layout(dir.path());
    match result {
        Err(Error::InvalidConfig { message }) => {
            assert!(message.contains("legacy") || message.contains("stores"));
        }
        other => panic!("expected InvalidConfig, got: {other:?}"),
    }
}

#[test]
fn validates_and_normalizes_oauth_public_url() {
    for value in [
        "https:host",
        "https:///host",
        "localdb.example.com",
        "ftp://host",
        "https://user:pass@host",
        "https://host/?q=x",
        "https://host/#x",
        "",
        "https://ho st",
    ] {
        let yaml = format!("version: 1\nserver:\n  public_url: {value:?}\n");
        assert!(load_config_from_str(&yaml).is_err(), "accepted {value}");
    }
    for (input, expected) in [
        ("https://db.example.com/", "https://db.example.com"),
        ("http://[::1]:7700/", "http://[::1]:7700"),
        ("https://host/localdb///", "https://host/localdb"),
    ] {
        let yaml = format!("version: 1\nserver:\n  public_url: {input:?}\n");
        assert_eq!(
            load_config_from_str(&yaml)
                .unwrap()
                .server
                .public_url
                .as_deref(),
            Some(expected)
        );
    }
}
