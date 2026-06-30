use std::path::Path;

use localdb_core::{
    config::loader::{load_config, LoadOptions},
    Error,
};
use serde_json::json;

use crate::{
    app_db::{default_store_row, load_app_db_lenient},
    daemon_client::CliContext,
    normalize::{exit_err, print_json},
};

/// `localdb init`
///
/// Creates config + data dir, writes default config if absent.
///
/// Strategy:
/// 1. Determine the config file path (from --config flag, LOCALDB_CONFIG env, or platform default).
/// 2. If the config file already exists, load it to get `paths.data`.
/// 3. Otherwise, use the platform default data dir.
/// 4. Write default config if absent.
/// 5. Create all directories.
/// 6. Initialize the runtime-state DB.
pub fn run_init(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_init_async(ctx));
}

pub(crate) async fn run_init_async(ctx: &CliContext) {
    let platform = localdb_core::config::PlatformPaths::resolve().unwrap_or_else(|| {
        eprintln!("error: cannot determine platform paths");
        std::process::exit(1);
    });

    // Resolve config path (same priority order as load_config).
    let config_path = ctx
        .config
        .clone()
        .or_else(|| ctx.config_env.clone())
        .unwrap_or_else(|| platform.config_file.clone());

    // F11: If --config was explicitly given but the parent directory doesn't exist,
    // fail with exit 2 (invalid config) rather than silently using platform defaults.
    if ctx.config.is_some() {
        if let Some(parent) = config_path.parent() {
            if !parent.exists() && parent != Path::new("") {
                exit_err(
                    &Error::InvalidConfig {
                        message: format!(
                            "config path parent directory '{}' does not exist",
                            parent.display()
                        ),
                    },
                    ctx.json,
                );
            }
        }
    }

    // If config exists, load it to get the resolved data dir.
    // If not, use platform defaults (we'll write the config shortly).
    let (data_dir, models_dir, logs_dir) = if config_path.exists() {
        let options = LoadOptions {
            config_path: Some(config_path.clone()),
            ..Default::default()
        };
        match load_config(&options, ctx.config_env.as_deref()) {
            Ok(cl) => (cl.paths.data_dir, cl.paths.models_dir, cl.paths.logs_dir),
            Err(_) => (
                platform.data_dir.clone(),
                platform.models_dir.clone(),
                platform.logs_dir.clone(),
            ),
        }
    } else {
        (
            platform.data_dir.clone(),
            platform.models_dir.clone(),
            platform.logs_dir.clone(),
        )
    };

    // Create directories.
    for dir in [
        config_path.parent().unwrap_or(Path::new(".")),
        &data_dir,
        &models_dir,
        &logs_dir,
    ] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            exit_err(
                &Error::Internal {
                    message: format!("cannot create directory '{}': {}", dir.display(), e),
                    correlation_id: "init_mkdir".to_string(),
                },
                ctx.json,
            );
        }
    }

    // Write default config if absent.
    if !config_path.exists() {
        let default_config =
            "version: 1\n# localdb configuration\n# Add stores and sources below.\n";
        if let Err(e) = std::fs::write(&config_path, default_config) {
            exit_err(
                &Error::Internal {
                    message: format!("cannot write config to '{}': {}", config_path.display(), e),
                    correlation_id: "init_config_write".to_string(),
                },
                ctx.json,
            );
        }
    }

    let (_config_loader, db) = load_app_db_lenient(ctx).await;

    match db.backend().get_store_by_name("default").await {
        Ok(None) => {
            let default_store = match default_store_row("default", &db) {
                Ok(store) => store,
                Err(e) => exit_err(&e, ctx.json),
            };
            if let Err(e) = db.backend().upsert_store(&default_store).await {
                exit_err(&e, ctx.json);
            }
        }
        Ok(Some(_)) => {}
        Err(e) => exit_err(&e, ctx.json),
    }

    if ctx.json {
        print_json(&json!({
            "status": "ok",
            "config_path": config_path.to_string_lossy(),
            "data_dir": data_dir.to_string_lossy(),
        }));
    } else {
        println!(
            "Initialized localdb at {}",
            config_path.parent().unwrap_or(Path::new(".")).display()
        );
        println!("  Config: {}", config_path.display());
        println!("  Data:   {}", data_dir.display());
        println!();
        println!(
            "Note: when using 'local-onnx' provider, the ONNX model is downloaded on first index."
        );
        println!("      Hosted providers (openai-compatible, perplexity, voyage) require an API key in config.");
        println!("Run `localdb store add <name>` to create a store.");
    }
}
