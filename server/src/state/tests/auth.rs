use super::common::make_state;
#[tokio::test]
async fn consume_setup_code_if_matches_only_on_exact_match() {
    let (_dir, state) = make_state().await;
    state.set_setup_code_hash("hash-of-real-code".to_string());

    assert!(
        !state.consume_setup_code_if_matches("wrong-hash"),
        "a mismatching guess must not consume the code"
    );
    assert_eq!(
        state.setup_code_hash().as_deref(),
        Some("hash-of-real-code"),
        "a failed guess leaves the hash in place for a legitimate retry"
    );

    assert!(state.consume_setup_code_if_matches("hash-of-real-code"));
    assert!(
        state.setup_code_hash().is_none(),
        "a matching presentation consumes (clears) the hash"
    );

    assert!(
        !state.consume_setup_code_if_matches("hash-of-real-code"),
        "the code cannot be redeemed a second time"
    );
}
