use super::*;

#[test]
fn router_schema_has_expected_envelope_keys() {
    let schema = generate_router_schema();

    assert_eq!(
        schema["$schema"],
        json!("https://json-schema.org/draft/2020-12/schema")
    );
    assert_eq!(schema["$id"], json!(SCHEMA_URL));
    assert!(schema.get("if").is_some(), "router schema must have `if`");
    assert!(
        schema.get("then").is_some(),
        "router schema must have `then`"
    );
    assert_eq!(
        schema["else"],
        json!(false),
        "unversioned/unknown-version configs must be rejected outright"
    );
    assert!(
        schema["$defs"]["v1"].is_object(),
        "$defs.v1 must be present"
    );
}

#[test]
fn router_schema_v1_matches_deny_unknown_fields() {
    let schema = generate_router_schema();
    let defs = schema["$defs"]
        .as_object()
        .expect("$defs must be an object");

    assert!(!defs.is_empty());
    for (name, def) in defs {
        if def["type"] != "object" {
            continue; // String enums have no object properties to constrain.
        }
        assert_eq!(
            def.get("additionalProperties"),
            Some(&json!(false)),
            "$defs.{name} must set additionalProperties: false, mirroring \
                 RawConfig's #[serde(deny_unknown_fields)]"
        );
    }
}

#[test]
fn router_schema_v1_admits_dollar_schema_property() {
    let schema = generate_router_schema();
    let props = schema["$defs"]["v1"]["properties"]
        .as_object()
        .expect("$defs.v1.properties must be an object");

    assert!(
        props.contains_key("$schema"),
        "v1 sub-schema must admit RawConfig's `$schema` editor-hint property"
    );
}

#[test]
fn router_schema_nested_refs_resolve() {
    let schema = generate_router_schema();
    let defs = schema["$defs"]
        .as_object()
        .expect("$defs must be an object");

    let mut refs = Vec::new();
    collect_refs(&schema, &mut refs);
    assert!(!refs.is_empty(), "expected at least one $ref in the schema");

    for r in refs {
        let name = r
            .strip_prefix("#/$defs/")
            .unwrap_or_else(|| panic!("unexpected $ref shape (not a local $defs ref): {r}"));
        assert!(
            defs.contains_key(name),
            "dangling $ref {r}: no such key in top-level $defs"
        );
    }
}

fn collect_refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if k == "$ref" {
                    if let Value::String(s) = v {
                        out.push(s.clone());
                    }
                }
                collect_refs(v, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_refs(v, out);
            }
        }
        _ => {}
    }
}

/// Regression for a real editor/generator trap: `u32`'s derived range is
/// `minimum: 0`, but `validate_config` (`core/src/config/loader.rs`)
/// rejects `0` for both `rate_limit` fields at load time. Without the
/// `#[schemars(range(min = 1))]` attributes on `RateLimitConfig` in
/// `core/src/config/schema.rs`, the published schema would call `0`
/// valid for a config the loader then refuses.
#[test]
fn rate_limit_fields_have_minimum_one_not_schemars_default_zero() {
    let schema = generate_router_schema();
    let rate_limit = &schema["$defs"]["v1_RateLimitConfig"]["properties"];

    assert_eq!(
        rate_limit["requests_per_second"]["minimum"],
        json!(1),
        "requests_per_second must declare minimum: 1, matching validate_config's floor"
    );
    assert_eq!(
        rate_limit["burst"]["minimum"],
        json!(1),
        "burst must declare minimum: 1, matching validate_config's floor"
    );
}

/// Same trap as `rate_limit_fields_have_minimum_one_not_schemars_default_zero`
/// above, for `ServerConfig::job_workers` (issue #208): `usize`'s
/// derived range is `minimum: 0`, but `validate_config` rejects `0` at
/// load time. Without `#[schemars(range(min = 1))]` on `job_workers` in
/// `core/src/config/schema.rs`, the published schema would call `0`
/// valid for a config the loader then refuses.
#[test]
fn job_workers_has_minimum_one_not_schemars_default_zero() {
    let schema = generate_router_schema();
    let server = &schema["$defs"]["v1_ServerConfig"]["properties"];

    assert_eq!(
        server["job_workers"]["minimum"],
        json!(1),
        "job_workers must declare minimum: 1, matching validate_config's floor"
    );
}

#[test]
fn router_schema_else_branch_rejects_unknown_version() {
    let schema = generate_router_schema();
    let validator = jsonschema::draft202012::new(&schema)
        .expect("router schema must compile as a valid draft 2020-12 schema");

    assert!(
        !validator.is_valid(&json!({"version": 2})),
        "version 2 has no sub-schema yet and must be rejected"
    );
    assert!(
        validator.is_valid(&json!({"version": 1})),
        "a minimal v1 instance must validate"
    );
}
