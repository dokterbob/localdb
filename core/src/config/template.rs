//! Handwritten, commented default config template.
//!
//! [`render_default_config_template`] substitutes [`super::jsonschema::SCHEMA_URL`]
//! into the `{{SCHEMA_URL}}` placeholders baked into `config.template.yaml` and
//! returns the result verbatim, ready to write to disk on `localdb init`.
//!
//! The template is checked at test time against the same sources of truth
//! it documents: `RawConfig`'s parsed defaults (`super::loader::load_config_from_str`)
//! and the generated JSON Schema (`super::jsonschema::generate_router_schema`) —
//! see the tests below.

use super::jsonschema::SCHEMA_URL;

const TEMPLATE: &str = include_str!("config.template.yaml");

/// Render the default config template with `{{SCHEMA_URL}}` substituted for
/// the canonical schema URL.
pub fn render_default_config_template() -> String {
    TEMPLATE.replace("{{SCHEMA_URL}}", SCHEMA_URL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::jsonschema::generate_router_schema;
    use crate::config::loader::load_config_from_str;

    #[test]
    fn template_first_line_is_yaml_language_server_modeline() {
        let rendered = render_default_config_template();
        let first_line = rendered.lines().next().expect("template is non-empty");
        assert_eq!(
            first_line,
            format!("# yaml-language-server: $schema={SCHEMA_URL}")
        );
    }

    #[test]
    fn template_schema_url_matches_generator() {
        let rendered = render_default_config_template();
        assert!(
            !rendered.contains("{{SCHEMA_URL}}"),
            "rendered template must not contain the raw placeholder"
        );

        let occurrences = rendered.matches(SCHEMA_URL).count();
        assert_eq!(
            occurrences, 2,
            "SCHEMA_URL must appear exactly twice (modeline + `$schema:` key)"
        );
    }

    #[test]
    fn template_parses_to_default_raw_config() {
        let rendered = render_default_config_template();
        let parsed = load_config_from_str(&rendered).expect("template must parse and validate");
        assert_eq!(parsed.schema, Some(SCHEMA_URL.to_string()));

        let mut expected = load_config_from_str("version: 1\n").expect("minimal config parses");
        // Normalize the one field the template intentionally sets and the
        // minimal config leaves absent.
        expected.schema = Some(SCHEMA_URL.to_string());

        assert_eq!(
            parsed, expected,
            "template must parse to the same defaults as a minimal `version: 1` config, \
             apart from the `schema` field"
        );
    }

    #[test]
    fn template_every_v1_schema_property_mentioned() {
        let schema = generate_router_schema();
        let defs = schema["$defs"]
            .as_object()
            .expect("$defs must be an object");

        let mut keys: Vec<String> = Vec::new();
        for (def_name, def) in defs {
            if def_name != "v1" && !def_name.starts_with("v1_") {
                continue;
            }
            if let Some(props) = def.get("properties").and_then(|p| p.as_object()) {
                keys.extend(props.keys().cloned());
            }
        }
        assert!(!keys.is_empty(), "expected v1 schema properties to inspect");

        for key in keys {
            assert!(
                TEMPLATE.contains(&key),
                "schema property `{key}` is not mentioned anywhere in the template \
                 (as a live or commented-out key)"
            );
        }
    }

    #[test]
    fn template_validates_against_generated_router_schema() {
        let rendered = render_default_config_template();
        let yaml_value: serde_yaml::Value =
            serde_yaml::from_str(&rendered).expect("rendered template must be valid YAML");
        let json_value: serde_json::Value =
            serde_json::to_value(yaml_value).expect("YAML value must convert to JSON");

        let schema = generate_router_schema();
        let validator = jsonschema::draft202012::new(&schema)
            .expect("router schema must compile as a valid draft 2020-12 schema");

        let errors: Vec<String> = validator
            .iter_errors(&json_value)
            .map(|e| e.to_string())
            .collect();
        assert!(
            errors.is_empty(),
            "rendered template must validate against the generated router schema: {errors:?}"
        );
    }

    #[test]
    fn template_contains_literal_dollar_schema_key() {
        let rendered = render_default_config_template();
        assert!(
            rendered.lines().any(|line| line.starts_with("$schema: ")),
            "rendered template must contain a `$schema: ` key line"
        );
    }
}
