use super::*;

#[test]
fn raw_config_defaults() {
    let cfg: RawConfig = serde_yaml::from_str("version: 1").unwrap();
    assert_eq!(cfg.version, 1);
    assert_eq!(cfg.server.bind, "127.0.0.1");
    assert_eq!(cfg.server.port, 7700);
    assert!(cfg.providers.is_empty());
    assert_eq!(cfg.schema, None);
}

#[test]
fn raw_config_accepts_dollar_schema_key() {
    let yaml = "version: 1\n$schema: https://example.com/x.json\n";
    let cfg: RawConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(cfg.version, 1);
    assert_eq!(cfg.schema, Some("https://example.com/x.json".to_string()));
}

#[test]
fn unknown_key_at_root_rejected() {
    let yaml = "version: 1\nunknown_field: foo\n";
    let result: Result<RawConfig, _> = serde_yaml::from_str(yaml);
    assert!(result.is_err(), "unknown root key should be rejected");
}

#[test]
fn unknown_key_in_server_rejected() {
    let yaml = "version: 1\nserver:\n  bind: 127.0.0.1\n  port: 7700\n  typo_field: bad\n";
    let result: Result<RawConfig, _> = serde_yaml::from_str(yaml);
    assert!(result.is_err(), "unknown server key should be rejected");
}

#[test]
fn raw_config_defaults_include_http() {
    let cfg: RawConfig = serde_yaml::from_str("version: 1").unwrap();
    assert_eq!(cfg.http, HttpConfig::default());
    assert_eq!(cfg.http.user_agent, None);
    assert_eq!(cfg.http.max_retries, 3);
    assert_eq!(cfg.http.rate_limit.requests_per_second, 1);
    assert_eq!(cfg.http.rate_limit.burst, 4);
}

#[test]
fn http_config_defaults() {
    let h = HttpConfig::default();
    assert_eq!(h.user_agent, None);
    assert_eq!(h.max_retries, 3);
    assert_eq!(h.rate_limit, RateLimitConfig::default());
}

#[test]
fn rate_limit_config_defaults() {
    let r = RateLimitConfig::default();
    assert_eq!(r.requests_per_second, 1);
    assert_eq!(r.burst, 4);
}

#[test]
fn unknown_key_in_http_rejected() {
    let yaml = "version: 1\nhttp:\n  max_retries: 3\n  typo_field: bad\n";
    let result: Result<RawConfig, _> = serde_yaml::from_str(yaml);
    assert!(result.is_err(), "unknown http key should be rejected");
}

#[test]
fn unknown_key_in_http_rate_limit_rejected() {
    let yaml =
        "version: 1\nhttp:\n  rate_limit:\n    requests_per_second: 1\n    typo_field: bad\n";
    let result: Result<RawConfig, _> = serde_yaml::from_str(yaml);
    assert!(
        result.is_err(),
        "unknown http.rate_limit key should be rejected"
    );
}

#[test]
fn raw_config_default_matches_bare_version_1() {
    // Default::default() must agree with parsing a minimal config, since
    // work item 2 relies on `..Default::default()` at every literal
    // construction site standing in for "every field at its platform
    // default" exactly as a bare `version: 1` config would produce.
    let parsed: RawConfig = serde_yaml::from_str("version: 1").unwrap();
    assert_eq!(parsed, RawConfig::default());
}

#[test]
fn embedding_policy_defaults() {
    let p = EmbeddingPolicy::default();
    assert_eq!(p.model, "pplx-embed-context-v1-0.6b");
    assert_eq!(p.provider, "local");
}

#[test]
fn server_config_defaults() {
    let s = ServerConfig::default();
    assert_eq!(s.bind, "127.0.0.1");
    assert_eq!(s.port, 7700);
    assert_eq!(s.job_workers, 1);
    assert_eq!(s.auth, ServerAuthMode::Auto);
    assert_eq!(s.public_url, None);
}

#[test]
fn server_auth_mode_parses_all_values() {
    for (yaml_value, expected) in [
        ("auto", ServerAuthMode::Auto),
        ("required", ServerAuthMode::Required),
        ("off", ServerAuthMode::Off),
    ] {
        let yaml = format!("version: 1\nserver:\n  auth: {yaml_value}\n");
        let cfg: RawConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(cfg.server.auth, expected, "auth: {yaml_value}");
    }
}

#[test]
fn server_auth_mode_defaults_to_auto_when_absent() {
    let cfg: RawConfig = serde_yaml::from_str("version: 1\n").unwrap();
    assert_eq!(cfg.server.auth, ServerAuthMode::Auto);
}

#[test]
fn server_auth_mode_rejects_unknown_value() {
    let yaml = "version: 1\nserver:\n  auth: sometimes\n";
    let result: Result<RawConfig, _> = serde_yaml::from_str(yaml);
    assert!(result.is_err(), "unknown auth mode should be rejected");
}

#[test]
fn server_public_url_parses() {
    let yaml = "version: 1\nserver:\n  public_url: https://localdb.example.com\n";
    let cfg: RawConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(
        cfg.server.public_url.as_deref(),
        Some("https://localdb.example.com")
    );
}

/// This list is duplicated in `extract::registry::default_parser_ids`
/// (core cannot depend on extract). The two must stay byte-for-byte
/// identical: the order feeds the policy-version hash and the chain's
/// first-match priority. Update both lists together.
#[test]
fn default_parser_ids_match_extract_registry() {
    assert_eq!(
        default_parser_ids(),
        vec!["pdf", "epub", "office", "html", "markdown", "plaintext"],
        "schema default_parser_ids must match extract::registry::default_parser_ids"
    );
}
