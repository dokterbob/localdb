use super::*;

#[test]
fn public_cli_context_can_be_constructed() {
    let ctx = CliContext {
        config: None,
        json: false,
        stores: vec![],
        yes: false,
        daemon_url: None,
        config_env: None,
    };
    assert!(!ctx.json);
}
