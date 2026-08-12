use std::path::Path;

use serde_json::json;

use crate::{
    app_db::{load_app_db_lenient, reject_store_flag, INIT_REJECT_MESSAGE},
    daemon_client::CliContext,
    normalize::{exit_err, print_json},
    scaffold::{ensure_config_scaffolded, ensure_default_store},
};

/// `localdb init`
///
/// A thin, explicit alias of the same first-run scaffolding every other
/// command now performs implicitly (issue #119/#120): writes the default
/// commented config template + creates the data/models/logs directories if
/// the config is genuinely absent (via [`ensure_config_scaffolded`] — no-op,
/// existing bytes untouched, if a config file is already there, even a
/// malformed one), then ensures a `default` store exists (via
/// [`ensure_default_store`]).
///
/// Unlike the implicit paths (`app_db::load_config_scaffolded`/
/// `load_config_lenient`, which only call `ensure_default_store` when
/// scaffolding *just* happened), `init`'s store check and directory
/// re-creation are unconditional — repair semantics: an operator who runs
/// `init` again after hand-editing their setup or deleting a directory the
/// config references (e.g. `paths.models` after a disk cleanup) should end
/// up with every piece recreated, not just a re-verified config file.
pub fn run_init(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_init_async(ctx));
}

pub(crate) async fn run_init_async(ctx: &CliContext) {
    // specs/05-surfaces.md §2.2: `init` runs before any store exists — the
    // only store it creates is `default`, and `-s` cannot rename or redirect
    // that. First statement in the function so a misused flag never creates
    // directories or writes a config first.
    reject_store_flag(ctx, INIT_REJECT_MESSAGE);

    let scaffold = match ensure_config_scaffolded(ctx).await {
        Ok(s) => s,
        Err(e) => exit_err(&e, ctx.json),
    };

    // Repair semantics: scaffolding only creates directories when it writes a
    // fresh config, but `init` recreates them even against an existing config
    // (whose `paths.*` overrides `scaffold` already resolved).
    for dir in [
        scaffold.config_path.parent().unwrap_or(Path::new(".")),
        &scaffold.data_dir,
        &scaffold.models_dir,
        &scaffold.logs_dir,
    ] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            exit_err(
                &localdb_core::Error::InvalidConfig {
                    message: format!("cannot create directory '{}': {e}", dir.display()),
                },
                ctx.json,
            );
        }
    }

    let (_config_loader, db) = load_app_db_lenient(ctx).await;
    if let Err(e) = ensure_default_store(&db).await {
        exit_err(&e, ctx.json);
    }

    if ctx.json {
        print_json(&json!({
            "status": "ok",
            "config_path": scaffold.config_path.to_string_lossy(),
            "data_dir": scaffold.data_dir.to_string_lossy(),
        }));
    } else {
        println!(
            "Initialized localdb at {}",
            scaffold
                .config_path
                .parent()
                .unwrap_or(Path::new("."))
                .display()
        );
        println!("  Config: {}", scaffold.config_path.display());
        println!("  Data:   {}", scaffold.data_dir.display());
        println!();
        println!(
            "Note: the default 'local' provider downloads its embedding model on first index."
        );
        println!("      Hosted providers (openai-compatible, perplexity, voyage) require an API key in config.");
        println!("Run `localdb store add <name>` to create a store.");
    }
}
