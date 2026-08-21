use std::path::Path;

use serde_json::json;

use crate::{
    app_db::{
        load_config_lenient, open_app_db_from_loader, reject_store_flag, INIT_REJECT_MESSAGE,
    },
    daemon_client::CliContext,
    normalize::{exit_err, print_json},
    scaffold::{ensure_config_scaffolded, ensure_default_store},
};

/// `localdb init`
///
/// Optional, explicit bootstrap: writes the default commented config
/// template + creates the data/models/logs directories if the config is
/// genuinely absent (via [`ensure_config_scaffolded`] — no-op, existing
/// bytes untouched, if a config file is already there, even a malformed
/// one), prints every resolved path, and — best-effort — ensures a
/// `default` store exists (via [`ensure_default_store`]).
///
/// Never required: every other command scaffolds the same config and
/// directories on first use (issue #119/#120). `init` exists for operators
/// who want to inspect/prepare their setup before running anything else.
///
/// Unlike the implicit paths (`app_db::load_config_scaffolded`/
/// `load_config_lenient`, which only call `ensure_default_store` when
/// scaffolding *just* happened), `init`'s store check and directory
/// re-creation are unconditional — repair semantics: an operator who runs
/// `init` again after hand-editing their setup or deleting a directory the
/// config references (e.g. `paths.models` after a disk cleanup) should end
/// up with every piece recreated, not just a re-verified config file.
///
/// The DB may legitimately fail to open here — most commonly because it
/// needs a schema migration (`localdb db migrate`), but also a locked or
/// corrupt file; no classification is attempted. `init`'s real job is the
/// config + directories, so any open failure is a warning, not a hard exit:
/// `default_store` reports `"skipped"` and the error text is surfaced via
/// `warnings` (`--json`) / `Warning:` lines on stderr (human output), while
/// the command itself still exits 0. When `download_model` is set,
/// `embed::create_embedder` is called unconditionally (no provider-name
/// special-casing — see issue #225) to prepare the configured embedder up
/// front; for a local provider that triggers the one-time ~706 MB download
/// now instead of on the first `index`/`search`, for a hosted provider it
/// just validates the client can be constructed.
pub fn run_init(ctx: &CliContext, download_model: bool) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_init_async(ctx, download_model));
}

pub(crate) async fn run_init_async(ctx: &CliContext, download_model: bool) {
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

    // `load_config_lenient` cannot hard-exit for `init`: its internal seed
    // path only fires when scaffolding *just* happened (`was_scaffolded`) or
    // when there is no `localdb.db` file yet *and* the config is still the
    // pristine template. `ensure_config_scaffolded` already ran above, so
    // `was_scaffolded` is false on this call; and the old/too-new-schema case
    // this function exists to tolerate is by definition a case where a
    // `localdb.db` file already exists. Both disjuncts are false, so the
    // internal `open_app_db_lenient_or_exit`/`exit_err` inside
    // `load_config_lenient` is unreachable from `init`.
    let loader = load_config_lenient(ctx).await;

    // Optional: prepare the embedder up front. `create_embedder` already
    // dispatches every provider and performs whatever download is needed —
    // no provider-name special-casing here (issue #225): for a hosted
    // provider this just validates the client can be constructed.
    let model_download = if download_model {
        eprintln!("Preparing embedding model (a local model downloads ~706 MB on first use)…");
        match embed::create_embedder(
            &loader.config.defaults.indexing.embedding,
            &loader.config.providers,
            Some(&loader.paths.models_dir),
            &(&loader.config.http).into(),
        ) {
            Ok(_) => "ok",
            Err(e) => exit_err(&localdb_core::Error::from(e), ctx.json),
        }
    } else {
        "skipped"
    };

    // The DB may legitimately be unopenable (most often: it needs a
    // migration). That must not fail `init`, whose real job is the config +
    // directories — warn on *any* open error, no classification, and exit 0
    // regardless. `open_app_db_from_loader`'s error already carries the
    // actionable "run `localdb db migrate`" text for the schema case.
    let (default_store, warnings) = match open_app_db_from_loader(&loader).await {
        Ok(db) => match ensure_default_store(&db).await {
            Ok(()) => ("ok", vec![]),
            Err(e) => ("skipped", vec![e.to_string()]),
        },
        Err(e) => ("skipped", vec![e.to_string()]),
    };

    if ctx.json {
        print_json(&json!({
            "status": "ok",
            "config_path": scaffold.config_path.to_string_lossy(),
            "data_dir": scaffold.data_dir.to_string_lossy(),
            "models_dir": scaffold.models_dir.to_string_lossy(),
            "logs_dir": scaffold.logs_dir.to_string_lossy(),
            "default_store": default_store,
            "model_download": model_download,
            "warnings": warnings,
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
        println!("  Models: {}", scaffold.models_dir.display());
        println!("  Logs:   {}", scaffold.logs_dir.display());
        println!();
        if model_download != "ok" {
            println!(
                "Note: the default 'local' provider downloads its embedding model on first index."
            );
            println!("      Hosted providers (openai-compatible, perplexity, voyage) require an API key in config.");
        }
        if default_store != "skipped" {
            println!("Run `localdb store add <name>` to create a store.");
        }
        for w in &warnings {
            eprintln!("Warning: {w}");
        }
    }
}
