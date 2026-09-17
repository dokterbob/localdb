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
        api_key: None,
    };
    assert!(!ctx.json);
}

pub(crate) fn context() -> CliContext {
    CliContext {
        config: None,
        json: false,
        stores: vec![],
        yes: false,
        daemon_url: None,
        config_env: None,
        api_key: None,
    }
}
