//! `localdb` — local-first knowledge server.
//!
//! Single binary with subcommands for all surfaces:
//! CLI, MCP server, and HTTP API daemon.
//!
//! See specs/05-surfaces.md §2 for the full subcommand table.

use clap::{Parser, Subcommand};
use cli::CliContext;

/// localdb — local-first knowledge server with hybrid search.
///
/// Indexes your files and URLs into a local store. Search with
/// natural language. Expose as an MCP server for AI agents.
/// Optionally run as a daemon with a REST API and file watching.
#[derive(Debug, Parser)]
#[command(
    name = "localdb",
    version,
    about = "Local-first knowledge server with hybrid search",
    long_about = None,
    propagate_version = true,
)]
pub struct Cli {
    /// Path to config file (default: platform data dir / localdb / config.yaml).
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<std::path::PathBuf>,

    /// Emit JSON output instead of human-readable text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Operate on this store (repeatable; defaults to all stores).
    #[arg(long = "store", short = 's', global = true, value_name = "NAME")]
    pub stores: Vec<String>,

    /// Skip confirmation prompts for destructive operations.
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// Top-level subcommands.
///
/// See specs/05-surfaces.md §2.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize config and data directory; prompt for first-run model download.
    Init,

    /// Start the HTTP API daemon (file watching, scheduled refresh, REST API).
    Serve,

    /// Run the MCP server on stdio for use with AI agents.
    Mcp {
        /// Enable write tools (reserved for future use; always rejected in v1).
        ///
        /// Parsing this flag now makes the CLI stable for callers even though
        /// the server rejects all mutating operations in v1.
        #[arg(long)]
        allow_write: bool,
    },

    /// Show stores, counts, policy staleness, and daemon state.
    Status,

    /// Manage stores.
    #[command(subcommand)]
    Store(StoreCommand),

    /// Manage sources on a store.
    #[command(subcommand)]
    Source(SourceCommand),

    /// Run a one-shot scan-and-index job.
    Index {
        /// Limit to a specific source (by ID).
        #[arg(long, value_name = "SOURCE_ID")]
        source: Option<String>,

        /// Index an arbitrary directory (creates a temporary anonymous source).
        #[arg(long, value_name = "PATH")]
        dir: Option<String>,

        /// Exit with code 2 if any document failed extraction (never aborts mid-run).
        #[arg(long)]
        strict: bool,
    },

    /// Hybrid search with citations.
    Search {
        /// Natural language query (no quotes needed; everything after the
        /// options is treated as the query).
        #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
        query: Vec<String>,

        /// Maximum number of results to return (must be >= 1).
        #[arg(long, default_value = "3", value_parser = clap::value_parser!(usize))]
        limit: usize,

        /// Max characters of snippet text shown per result in human-readable output.
        #[arg(long, default_value = "1000", value_parser = clap::value_parser!(usize))]
        content_length: usize,
    },

    /// Alias for `source add`: add one or more sources to a store.
    Add {
        /// Source paths or URLs (one or more).
        #[arg(required = true, num_args = 1..)]
        sources: Vec<String>,
    },
}

/// Store management subcommands.
#[derive(Debug, Subcommand)]
pub enum StoreCommand {
    /// Add a new store.
    Add {
        /// Store name.
        name: String,
    },
    /// List all stores.
    List,
    /// Remove a store.
    Remove {
        /// Store name or ID.
        name: String,
    },
}

/// Source management subcommands.
#[derive(Debug, Subcommand)]
pub enum SourceCommand {
    /// Add a new source to a store.
    Add {
        /// Source paths or URLs (one or more).
        #[arg(required = true, num_args = 1..)]
        sources: Vec<String>,
    },
    /// List sources on a store.
    List,
    /// Remove a source from a store.
    Remove {
        /// Source IDs, paths, or URLs (one or more).
        #[arg(required = true, num_args = 1..)]
        ids: Vec<String>,
    },
}

fn main() {
    // Initialize structured logging. In embedded mode (no daemon), emit to stderr.
    // pdf-extract/lopdf emit high-volume WARN noise (unknown glyph, corrupt deflate,
    // Unicode mismatch) that is not actionable — quiet those targets to `error`.
    // Real per-document extraction failures surface via the job outcome path, not here.
    // RUST_LOG still overrides this default entirely (e.g. RUST_LOG=debug to see it all).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("warn,pdf_extract=error,lopdf=error")
            }),
        )
        .init();

    let cli = Cli::parse();

    let ctx = CliContext {
        config: cli.config,
        json: cli.json,
        stores: cli.stores,
        yes: cli.yes,
        daemon_url: std::env::var("LOCALDB_DAEMON_URL")
            .ok()
            .filter(|s| !s.is_empty()),
        config_env: std::env::var("LOCALDB_CONFIG")
            .ok()
            .map(std::path::PathBuf::from),
    };

    match &cli.command {
        Command::Init => cli::run_init(&ctx),
        Command::Serve => cli::run_serve(&ctx),
        Command::Mcp { allow_write } => cli::run_mcp(&ctx, *allow_write),
        Command::Status => cli::run_status(&ctx),
        Command::Store(cmd) => match cmd {
            StoreCommand::Add { name } => cli::run_store_add(&ctx, name),
            StoreCommand::List => cli::run_store_list(&ctx),
            StoreCommand::Remove { name } => cli::run_store_remove(&ctx, name),
        },
        Command::Source(cmd) => match cmd {
            SourceCommand::Add { sources } => {
                // #5: loop over multiple arguments.
                for source in sources {
                    cli::run_source_add(&ctx, source);
                }
            }
            SourceCommand::List => cli::run_source_list(&ctx),
            SourceCommand::Remove { ids } => {
                // #5: loop over multiple arguments.
                for id in ids {
                    cli::run_source_remove(&ctx, id);
                }
            }
        },
        Command::Index {
            source,
            dir,
            strict,
        } => cli::run_index(&ctx, source.as_deref(), dir.as_deref(), *strict),
        Command::Search {
            query,
            limit,
            content_length,
        } => cli::run_search(&ctx, &query.join(" "), *limit, *content_length),
        Command::Add { sources } => {
            for source in sources {
                cli::run_source_add(&ctx, source);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the CLI can be parsed without panicking.
    #[test]
    fn cli_help_parses() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    /// Verify all top-level subcommand names from specs/05-surfaces.md §2.
    #[test]
    fn all_subcommands_present() {
        use clap::CommandFactory;
        let cmd = Cli::command();
        let subcommand_names: Vec<&str> = cmd.get_subcommands().map(|sc| sc.get_name()).collect();

        for expected in &[
            "init", "serve", "mcp", "status", "store", "source", "index", "search", "add",
        ] {
            assert!(
                subcommand_names.contains(expected),
                "subcommand '{}' is missing from the CLI; found: {:?}",
                expected,
                subcommand_names,
            );
        }
    }

    /// Verify the store subcommands are present.
    #[test]
    fn store_subcommands_present() {
        use clap::CommandFactory;
        let cmd = Cli::command();
        let store_cmd = cmd
            .get_subcommands()
            .find(|sc| sc.get_name() == "store")
            .expect("store subcommand missing");

        let sub_names: Vec<&str> = store_cmd
            .get_subcommands()
            .map(|sc| sc.get_name())
            .collect();

        for expected in &["add", "list", "remove"] {
            assert!(
                sub_names.contains(expected),
                "store {expected} subcommand missing; found: {sub_names:?}",
            );
        }
    }

    /// Verify the source subcommands are present.
    #[test]
    fn source_subcommands_present() {
        use clap::CommandFactory;
        let cmd = Cli::command();
        let source_cmd = cmd
            .get_subcommands()
            .find(|sc| sc.get_name() == "source")
            .expect("source subcommand missing");

        let sub_names: Vec<&str> = source_cmd
            .get_subcommands()
            .map(|sc| sc.get_name())
            .collect();

        for expected in &["add", "list", "remove"] {
            assert!(
                sub_names.contains(expected),
                "source {expected} subcommand missing; found: {sub_names:?}",
            );
        }
    }

    /// Unquoted multi-word query is joined into a single string.
    #[test]
    fn search_query_trailing_var_arg() {
        let cli = Cli::try_parse_from(["localdb", "search", "machine", "learning"]).unwrap();
        if let Command::Search {
            query,
            limit,
            content_length,
        } = cli.command
        {
            assert_eq!(query.join(" "), "machine learning");
            assert_eq!(limit, 3);
            assert_eq!(content_length, 1000);
        } else {
            panic!("expected Search command");
        }
    }

    /// `localdb add <path>` parses to Command::Add.
    #[test]
    fn add_alias_parses() {
        let cli = Cli::try_parse_from(["localdb", "add", "/some/path"]).unwrap();
        if let Command::Add { sources } = cli.command {
            assert_eq!(sources, vec!["/some/path"]);
        } else {
            panic!("expected Add command");
        }
    }

    /// `-s` short flag populates `stores`.
    #[test]
    fn short_store_flag() {
        let cli = Cli::try_parse_from(["localdb", "-s", "notes", "search", "foo"]).unwrap();
        assert_eq!(cli.stores, vec!["notes"]);
    }

    /// `-s` short flag works as a subcommand-level option too.
    #[test]
    fn short_store_flag_after_subcommand() {
        let cli = Cli::try_parse_from(["localdb", "search", "-s", "notes", "neural", "networks"])
            .unwrap();
        assert_eq!(cli.stores, vec!["notes"]);
        if let Command::Search { query, .. } = cli.command {
            assert_eq!(query.join(" "), "neural networks");
        } else {
            panic!("expected Search command");
        }
    }

    /// Verify global flags exist.
    #[test]
    fn global_flags_present() {
        use clap::CommandFactory;
        let cmd = Cli::command();
        let arg_names: Vec<&str> = cmd.get_arguments().map(|a| a.get_id().as_str()).collect();

        assert!(arg_names.contains(&"config"), "missing --config flag");
        assert!(arg_names.contains(&"json"), "missing --json flag");
        assert!(arg_names.contains(&"stores"), "missing --store flag");
        assert!(arg_names.contains(&"yes"), "missing --yes/-y flag");
    }
}
