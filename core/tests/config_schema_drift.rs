//! Drift guard: the committed `schema/config.schema.json` artifact must be
//! byte-identical to what `generate_router_schema()` produces right now.
//!
//! The artifact is regenerated with `localdb internal print-schema`
//! (`cli/src/cmds/internal.rs`, hidden CLI subcommand) — this test asserts
//! nobody edited the committed file by hand, and nobody changed the
//! generator without regenerating it.

use localdb_core::config::generate_router_schema;

#[test]
fn drift_guard_committed_schema_matches_generator() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../schema/config.schema.json");
    let committed = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read committed schema at {path}: {e}"));

    // Must match exactly what `localdb internal print-schema` emits:
    // `serde_json::to_string_pretty` plus a trailing newline (`println!`).
    let generated = format!(
        "{}\n",
        serde_json::to_string_pretty(&generate_router_schema()).expect("schema serializes to JSON")
    );

    assert_eq!(
        committed, generated,
        "schema/config.schema.json is stale — regenerate it with:\n\
         TMPDIR=\"/Volumes/User Home/dev/tmp\" cargo run -p localdb -- internal print-schema > schema/config.schema.json"
    );
}
