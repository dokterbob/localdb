//! `preset_for` routing tests.

use crate::chunker::preset_for;

// ---------------------------------------------------------------------------
// Layer A: preset_for routing tests
// ---------------------------------------------------------------------------

#[test]
fn preset_for_routes_code_extensions() {
    assert_eq!(preset_for(Some("lib.rs"), None), "code");
    assert_eq!(preset_for(Some("data.json"), None), "code");
    assert_eq!(preset_for(Some("config.toml"), None), "code");
    assert_eq!(preset_for(Some("Cargo.lock"), None), "code");
    assert_eq!(preset_for(None, Some("application/json")), "code");
    assert_eq!(preset_for(None, Some("text/x-rust")), "code");
}

#[test]
fn preset_for_routes_prose() {
    assert_eq!(preset_for(Some("README.md"), None), "prose");
    assert_eq!(preset_for(Some("notes.txt"), None), "prose");
    assert_eq!(preset_for(Some("page.html"), None), "prose");
    assert_eq!(preset_for(Some("doc.pdf"), None), "prose");
    assert_eq!(preset_for(None, Some("text/plain")), "prose");
}
