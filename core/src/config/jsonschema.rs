//! Versioned "router" JSON Schema generator for `config.yaml`.
//!
//! [`generate_router_schema`] produces a single schema document that
//! dispatches on the top-level `version` field to a per-version
//! sub-schema (`$defs.v1`, `$defs.v2`, …), derived from the corresponding
//! [`RawConfig`]-shaped Rust types via `schemars`. Editors and validators
//! can point at one stable URL ([`SCHEMA_URL`]) regardless of which config
//! version is in use; an unrecognized `version` fails validation outright
//! (the `else: false` branch), rather than silently validating against
//! nothing.
//!
//! See specs/03-config.md.

use super::schema::RawConfig;
use schemars::{JsonSchema, SchemaGenerator};
use serde_json::{json, Map, Value};

/// Canonical URL this schema is published at. Used as the document's
/// `$id`, and as the value config authors put in their `$schema:` key.
pub const SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/dokterbob/localdb/main/schema/config.schema.json";

/// Generate the versioned "router" JSON Schema for `config.yaml`.
///
/// `version: 1` dispatches to the `v1` sub-schema (derived from
/// [`RawConfig`]); any other `version` value fails validation.
pub fn generate_router_schema() -> Value {
    let defs = versioned_subschema::<RawConfig>("v1");

    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": SCHEMA_URL,
        "title": "localdb configuration",
        "description": "config.yaml for localdb. Dispatches on the top-level `version` field to a per-version sub-schema; see specs/03-config.md.",
        "type": "object",
        "required": ["version"],
        "properties": {
            "version": {
                "type": "integer",
                "description": "Config schema version. Currently only 1 is supported."
            }
        },
        "if": {
            "properties": { "version": { "const": 1 } },
            "required": ["version"]
        },
        "then": { "$ref": "#/$defs/v1" },
        "else": false,
        "$defs": Value::Object(defs),
    })
}

/// Turn a `schemars`-derived root schema for `T` into a flat set of
/// `$defs` entries suitable for splicing into a router schema, all keyed
/// under `prefix`.
///
/// `schemars::SchemaGenerator::into_root_schema_for::<T>()` inlines `T`'s
/// object schema at the root and collects every nested struct/enum it
/// references into a root-level `$defs` map, keyed by bare type name
/// (e.g. `$defs.ServerConfig`), with internal refs of the form
/// `"#/$defs/ServerConfig"`.
///
/// This helper:
/// 1. removes the root-level `$defs` map from the root schema,
/// 2. strips the root-level `$schema` / `$id` meta-keys (NOT a property
///    that happens to be *named* `$schema`, e.g. `RawConfig.schema` —
///    only the meta-keys living beside `type`/`properties` are removed),
/// 3. renames every nested def key `K` -> `{prefix}_K`,
/// 4. rewrites every `"#/$defs/K"` ref string anywhere in the tree
///    (root schema and nested defs alike) to `"#/$defs/{prefix}_K"`,
/// 5. returns a map with the (now ref-rewritten) root schema stored
///    under `prefix` itself, alongside the renamed nested defs.
///
/// The caller is expected to splice the returned map into its own
/// `$defs`. Generic over `T` and `prefix` so a future `v2` can reuse it
/// unmodified: `versioned_subschema::<RawConfigV2>("v2")`.
fn versioned_subschema<T: JsonSchema>(prefix: &str) -> Map<String, Value> {
    let schema = SchemaGenerator::default().into_root_schema_for::<T>();
    let mut root = match schema.to_value() {
        Value::Object(map) => map,
        other => panic!("schemars root schema for a struct must be a JSON object, got {other:?}"),
    };

    let nested_defs = match root.remove("$defs") {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };

    // Root-level meta-keys only make sense on a standalone document, not
    // on a schema embedded as a `$defs` entry of the router schema.
    root.remove("$schema");
    root.remove("$id");

    let mut out = Map::new();
    out.insert(
        prefix.to_string(),
        rewrite_defs_refs(Value::Object(root), prefix),
    );
    for (name, def_schema) in nested_defs {
        out.insert(
            format!("{prefix}_{name}"),
            rewrite_defs_refs(def_schema, prefix),
        );
    }
    out
}

/// Recursively rewrite every `"#/$defs/K"` string anywhere in `value` to
/// `"#/$defs/{prefix}_K"`.
fn rewrite_defs_refs(value: Value, prefix: &str) -> Value {
    match value {
        Value::String(s) => match s.strip_prefix("#/$defs/") {
            Some(rest) => Value::String(format!("#/$defs/{prefix}_{rest}")),
            None => Value::String(s),
        },
        Value::Array(arr) => Value::Array(
            arr.into_iter()
                .map(|v| rewrite_defs_refs(v, prefix))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, rewrite_defs_refs(v, prefix)))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests;
