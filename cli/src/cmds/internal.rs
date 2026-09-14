//! Hidden `localdb internal` maintenance subcommands.
//!
//! These are build/release tooling, not part of the public surface
//! (specs/05-surfaces.md): they are marked `#[command(hide = true)]` in
//! `localdb/src/main.rs` and never appear in `--help`.

/// `localdb internal print-schema`
///
/// Prints the generated router JSON Schema for `config.yaml`
/// (`localdb_core::config::generate_router_schema()`) to stdout, exit 0.
///
/// Deliberately does none of the usual CLI setup — no config load, no
/// daemon probe, no `command_table::dispatch` — this is pure, offline
/// codegen used to (re)produce the committed `schema/config.schema.json`
/// artifact and to let `core/tests/config_schema_drift.rs` and
/// `localdb/tests/cli_integration.rs` assert it never drifts.
pub fn run_internal_print_schema() {
    let schema = localdb_core::config::generate_router_schema();
    println!(
        "{}",
        serde_json::to_string_pretty(&schema).expect("schema serializes to JSON")
    );
}
