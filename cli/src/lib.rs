//! CLI command implementations for localdb.
//!
//! Thin layer on `core` — no business logic lives here (invariant from
//! specs/01-architecture.md §1). Each command acquires config + runtime state,
//! then calls into the core crates.

pub mod progress;

mod app_db;
mod daemon_client;
mod normalize;

mod cmds {
    pub(crate) mod index;
    pub(crate) mod init;
    pub(crate) mod search;
    pub(crate) mod source;
    pub(crate) mod status;
    pub(crate) mod store;
    pub(crate) mod surface;
}

pub use app_db::AppDb;
pub use cmds::index::run_index;
pub use cmds::init::run_init;
pub use cmds::search::run_search;
pub use cmds::source::{run_source_add, run_source_list, run_source_remove};
pub use cmds::status::run_status;
pub use cmds::store::{run_store_add, run_store_list, run_store_remove};
pub use cmds::surface::{run_mcp, run_serve};
pub use daemon_client::{probe_daemon, CliContext, DaemonState};
pub use normalize::{
    classify_source, confirm_destructive, exit_err, source_row_to_core_source, validate_store_name,
};

#[cfg(test)]
mod tests {
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
}
