use super::common::make_state;

#[tokio::test]
async fn config_edits_do_not_change_startup_settings() {
    let (dir, state) = make_state().await;
    let before = state.yaml_config().server.port;
    std::fs::write(
        dir.path().join("config.yaml"),
        "version: 1\nserver:\n  port: 12345\n",
    )
    .unwrap();
    assert_eq!(state.yaml_config().server.port, before);
}
