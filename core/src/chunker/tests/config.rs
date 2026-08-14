//! `ChunkerConfig` tests.

use crate::chunker::ChunkerConfig;

// ---------------------------------------------------------------------------
// ChunkerConfig tests
// ---------------------------------------------------------------------------

#[test]
fn chunker_config_prose_defaults() {
    let cfg = ChunkerConfig::prose();
    assert_eq!(cfg.preset, "prose");
    assert_eq!(cfg.resolved_target_tokens(), 256);
    assert_eq!(cfg.resolved_overlap_tokens(), 0);
}

#[test]
fn chunker_config_code_defaults() {
    let cfg = ChunkerConfig::code();
    assert_eq!(cfg.preset, "code");
    assert_eq!(cfg.resolved_target_tokens(), 3000);
}

#[test]
fn chunker_config_from_preset_prose() {
    let cfg = ChunkerConfig::from_preset("prose").unwrap();
    assert_eq!(cfg.preset, "prose");
}

#[test]
fn chunker_config_from_preset_code() {
    let cfg = ChunkerConfig::from_preset("code").unwrap();
    assert_eq!(cfg.preset, "code");
}

#[test]
fn chunker_config_from_preset_messages_succeeds() {
    let cfg = ChunkerConfig::from_preset("messages").unwrap();
    assert_eq!(cfg.preset, "messages");
    assert_eq!(cfg.resolved_window_turns(), 6);
    assert_eq!(cfg.resolved_stride_turns(), 3);
}

#[test]
fn chunker_config_from_preset_unknown_errors() {
    let result = ChunkerConfig::from_preset("unknown_preset");
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), "invalid_request");
}
