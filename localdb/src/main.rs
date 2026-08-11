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

    /// Operate on this store (repeatable); default depends on the subcommand.
    ///
    /// Omitted, this means "all stores" for `search`/`status`/`store list`/
    /// `index`; the store named `default` for `source`/`add` (exit 2 if
    /// absent); and is rejected outright for `db` subcommands (exit 2). See
    /// `--help` on the specific subcommand for its exact rule.
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
    ///
    /// `add`/`list`/`remove` default to the store named `default` when
    /// `--store` is omitted; exit 2 if no store named `default` exists.
    #[command(subcommand)]
    Source(SourceCommand),

    /// Inspect or migrate the database schema.
    ///
    /// Operates on the whole database file, not a single store: `--store` is
    /// rejected outright (exit 2) on all three subcommands.
    // See specs/05-surfaces.md §2.1.
    #[command(subcommand)]
    Db(DbCommand),

    /// Run a one-shot scan-and-index job.
    ///
    /// Indexes every store when `--store` is omitted; pass `--store <NAME>`
    /// (repeatable) to index only the named store(s).
    Index {
        /// Limit to a specific source (by ID).
        #[arg(long, value_name = "SOURCE_ID")]
        source: Option<String>,

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
    ///
    /// Defaults to the store named `default` when `--store` is omitted;
    /// exit 2 if no store named `default` exists.
    Add {
        /// Source paths or URLs (one or more).
        #[arg(required = true, num_args = 1..)]
        sources: Vec<String>,
        /// Refresh interval for URL and feed sources (e.g. "1h", "30m", "3600").
        #[arg(long)]
        refresh: Option<String>,
        /// Override source-kind classification instead of inferring it from
        /// the argument (path vs. `http(s)://` URL). `feed` treats the
        /// argument as an Atom/RSS feed URL, which fetches every entry page
        /// at index time — pass `--max-entries` to bound that.
        #[arg(long, value_enum)]
        kind: Option<SourceKindArg>,
        /// Cap on feed entries considered per indexing run (feed sources only).
        #[arg(long, value_name = "N")]
        max_entries: Option<u32>,
        /// For feed sources, index only the feed-supplied summary instead of
        /// fetching each entry's full page content (feed sources only).
        #[arg(long)]
        no_fetch_full_content: bool,
    },
}

/// `--kind` override for `source add` / `add` (see [`Command::Add`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SourceKindArg {
    Path,
    Url,
    Feed,
}

impl SourceKindArg {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SourceKindArg::Path => "path",
            SourceKindArg::Url => "url",
            SourceKindArg::Feed => "feed",
        }
    }
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

/// Schema-migration maintenance subcommands (specs/05-surfaces.md §2.1).
///
/// CLI-only: the HTTP daemon and MCP never apply migrations themselves —
/// they only ever surface the refusal-with-hint that `LibsqlDb::open`
/// produces on a version mismatch. All three subcommands refuse with
/// `daemon_running` (exit 4) while the daemon is up, the same way every
/// other daemon-aware write command does. None of them are store-scoped:
/// they operate on the whole database file, and `--store` is rejected
/// outright (exit 2) rather than silently ignored.
#[derive(Debug, Subcommand)]
pub enum DbCommand {
    /// Show schema version, pending migrations, and migration history.
    ///
    /// Never refuses, even on a store newer than this binary or one that
    /// predates the migration framework entirely. Not store-scoped: passing
    /// `--store` exits 2.
    Status,

    /// Apply pending migrations to bring the database up to this binary's head version.
    ///
    /// A legacy (pre-migration-framework, v1-v3) store requires confirmation
    /// before its destructive rebuild (all indexed data is lost); an
    /// ordinary forward migration needs no confirmation. Not store-scoped:
    /// passing `--store` exits 2.
    Migrate,

    /// Reverse migrations using stored down-SQL (default: one step back).
    ///
    /// Always requires confirmation. Refuses cleanly, without changing
    /// anything, if a migration on the way to `--to` has no down path. Not
    /// store-scoped: passing `--store` exits 2.
    Downgrade {
        /// Target schema version to downgrade to (default: one step below the current version).
        #[arg(long, value_name = "VERSION")]
        to: Option<i64>,
    },

    /// Reclaim disk space freed by prior migrations/deletes by rewriting the
    /// whole database file (SQLite `VACUUM`).
    ///
    /// A schema migration (e.g. v6 `shrink_vector_index`) or an ordinary
    /// bulk delete frees pages onto SQLite's own free list, but the file
    /// itself does not shrink until something rewrites it — this does that.
    /// Data-preserving (an interrupted VACUUM leaves the original file
    /// untouched), but needs roughly the current file size again in free
    /// disk space and can take minutes on a large store. Not store-scoped:
    /// passing `--store` exits 2.
    Vacuum,
}

/// Source management subcommands.
///
/// All three default to the store named `default` when `--store` is
/// omitted, and exit 2 if no store named `default` exists.
#[derive(Debug, Subcommand)]
pub enum SourceCommand {
    /// Add a new source to a store.
    ///
    /// Defaults to the store named `default` when `--store` is omitted;
    /// exit 2 if no store named `default` exists.
    Add {
        /// Source paths or URLs (one or more).
        #[arg(required = true, num_args = 1..)]
        sources: Vec<String>,
        /// Refresh interval for URL and feed sources (e.g. "1h", "30m", "3600").
        #[arg(long)]
        refresh: Option<String>,
        /// Override source-kind classification instead of inferring it from
        /// the argument (path vs. `http(s)://` URL). `feed` treats the
        /// argument as an Atom/RSS feed URL, which fetches every entry page
        /// at index time — pass `--max-entries` to bound that.
        #[arg(long, value_enum)]
        kind: Option<SourceKindArg>,
        /// Cap on feed entries considered per indexing run (feed sources only).
        #[arg(long, value_name = "N")]
        max_entries: Option<u32>,
        /// For feed sources, index only the feed-supplied summary instead of
        /// fetching each entry's full page content (feed sources only).
        #[arg(long)]
        no_fetch_full_content: bool,
    },
    /// List sources on a store.
    ///
    /// Defaults to the store named `default` when `--store` is omitted;
    /// exit 2 if no store named `default` exists.
    List,
    /// Remove a source from a store.
    ///
    /// Defaults to the store named `default` when `--store` is omitted;
    /// exit 2 if no store named `default` exists.
    Remove {
        /// Source IDs, paths, or URLs (one or more).
        #[arg(required = true, num_args = 1..)]
        ids: Vec<String>,
    },
}

fn main() {
    // Initialize structured logging. In embedded mode (no daemon), emit to stderr.
    // The PDF parser (pdf_oxide) emits high-volume, per-glyph/per-object noise
    // (unmappable glyphs, malformed streams, recovery warnings) that is not
    // actionable for users — suppress that target entirely by default. Real
    // per-document extraction failures surface via the job outcome path (one
    // WARN line per failed file), not here.
    // RUST_LOG still overrides this default entirely (e.g. RUST_LOG=debug to see it all).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,pdf_oxide=off")),
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
            SourceCommand::Add {
                sources,
                refresh,
                kind,
                max_entries,
                no_fetch_full_content,
            } => {
                // #5: loop over multiple arguments.
                for source in sources {
                    cli::run_source_add(
                        &ctx,
                        source,
                        refresh.as_deref(),
                        (*kind).map(SourceKindArg::as_str),
                        *max_entries,
                        *no_fetch_full_content,
                    );
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
        Command::Db(cmd) => match cmd {
            DbCommand::Status => cli::run_db_status(&ctx),
            DbCommand::Migrate => cli::run_db_migrate(&ctx),
            DbCommand::Downgrade { to } => cli::run_db_downgrade(&ctx, *to),
            DbCommand::Vacuum => cli::run_db_vacuum(&ctx),
        },
        Command::Index { source, strict } => cli::run_index(&ctx, source.as_deref(), *strict),
        Command::Search {
            query,
            limit,
            content_length,
        } => cli::run_search(&ctx, &query.join(" "), *limit, *content_length),
        Command::Add {
            sources,
            refresh,
            kind,
            max_entries,
            no_fetch_full_content,
        } => {
            for source in sources {
                cli::run_source_add(
                    &ctx,
                    source,
                    refresh.as_deref(),
                    (*kind).map(SourceKindArg::as_str),
                    *max_entries,
                    *no_fetch_full_content,
                );
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
            "init", "serve", "mcp", "status", "store", "source", "db", "index", "search", "add",
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

    /// Verify the db subcommands are present.
    #[test]
    fn db_subcommands_present() {
        use clap::CommandFactory;
        let cmd = Cli::command();
        let db_cmd = cmd
            .get_subcommands()
            .find(|sc| sc.get_name() == "db")
            .expect("db subcommand missing");

        let sub_names: Vec<&str> = db_cmd.get_subcommands().map(|sc| sc.get_name()).collect();

        for expected in &["status", "migrate", "downgrade", "vacuum"] {
            assert!(
                sub_names.contains(expected),
                "db {expected} subcommand missing; found: {sub_names:?}",
            );
        }
    }

    /// `localdb db vacuum` parses with no arguments.
    #[test]
    fn db_vacuum_parses() {
        assert!(matches!(
            Cli::try_parse_from(["localdb", "db", "vacuum"])
                .unwrap()
                .command,
            Command::Db(DbCommand::Vacuum)
        ));
    }

    /// `localdb db downgrade --to N` parses `N` as an `i64`.
    #[test]
    fn db_downgrade_to_flag_parses_i64() {
        let cli = Cli::try_parse_from(["localdb", "db", "downgrade", "--to", "3"]).unwrap();
        if let Command::Db(DbCommand::Downgrade { to }) = cli.command {
            assert_eq!(to, Some(3));
        } else {
            panic!("expected Db(Downgrade) command");
        }
    }

    /// `localdb db downgrade` without `--to` parses to `None` (CLI resolves
    /// the one-step-back default itself, not the library's baseline default).
    #[test]
    fn db_downgrade_without_to_defaults_to_none() {
        let cli = Cli::try_parse_from(["localdb", "db", "downgrade"]).unwrap();
        if let Command::Db(DbCommand::Downgrade { to }) = cli.command {
            assert_eq!(to, None);
        } else {
            panic!("expected Db(Downgrade) command");
        }
    }

    /// `localdb db status` and `localdb db migrate` parse with no arguments.
    #[test]
    fn db_status_and_migrate_parse() {
        assert!(matches!(
            Cli::try_parse_from(["localdb", "db", "status"])
                .unwrap()
                .command,
            Command::Db(DbCommand::Status)
        ));
        assert!(matches!(
            Cli::try_parse_from(["localdb", "db", "migrate"])
                .unwrap()
                .command,
            Command::Db(DbCommand::Migrate)
        ));
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
        if let Command::Add { sources, .. } = cli.command {
            assert_eq!(sources, vec!["/some/path"]);
        } else {
            panic!("expected Add command");
        }
    }

    /// `--kind`, `--max-entries`, `--no-fetch-full-content` parse on `add`.
    #[test]
    fn add_feed_flags_parse() {
        let cli = Cli::try_parse_from([
            "localdb",
            "add",
            "https://example.com/feed.xml",
            "--kind",
            "feed",
            "--max-entries",
            "10",
            "--no-fetch-full-content",
        ])
        .unwrap();
        if let Command::Add {
            kind,
            max_entries,
            no_fetch_full_content,
            ..
        } = cli.command
        {
            assert_eq!(kind, Some(SourceKindArg::Feed));
            assert_eq!(max_entries, Some(10));
            assert!(no_fetch_full_content);
        } else {
            panic!("expected Add command");
        }
    }

    /// Same flags parse identically on `source add`.
    #[test]
    fn source_add_feed_flags_parse() {
        let cli = Cli::try_parse_from([
            "localdb",
            "source",
            "add",
            "https://example.com/feed.xml",
            "--kind",
            "feed",
            "--max-entries",
            "10",
            "--no-fetch-full-content",
        ])
        .unwrap();
        if let Command::Source(SourceCommand::Add {
            kind,
            max_entries,
            no_fetch_full_content,
            ..
        }) = cli.command
        {
            assert_eq!(kind, Some(SourceKindArg::Feed));
            assert_eq!(max_entries, Some(10));
            assert!(no_fetch_full_content);
        } else {
            panic!("expected Source(Add) command");
        }
    }

    /// `--kind path|url` also parses (bypasses classification without a
    /// feed-only implication).
    #[test]
    fn kind_path_and_url_parse() {
        let cli = Cli::try_parse_from(["localdb", "add", "some-arg", "--kind", "path"]).unwrap();
        if let Command::Add { kind, .. } = cli.command {
            assert_eq!(kind, Some(SourceKindArg::Path));
        } else {
            panic!("expected Add command");
        }

        let cli = Cli::try_parse_from(["localdb", "add", "some-arg", "--kind", "url"]).unwrap();
        if let Command::Add { kind, .. } = cli.command {
            assert_eq!(kind, Some(SourceKindArg::Url));
        } else {
            panic!("expected Add command");
        }
    }

    /// `Command::Add` and `SourceCommand::Add` must expose identical arg
    /// names/requirements for the shared flags — they are hand-synced clap
    /// structs, and drift between them would silently desync `localdb add`
    /// from `localdb source add` (issue #116).
    #[test]
    fn add_and_source_add_flags_are_in_parity() {
        use clap::CommandFactory;
        use std::collections::BTreeMap;

        let cmd = Cli::command();
        let add_cmd = cmd
            .get_subcommands()
            .find(|sc| sc.get_name() == "add")
            .expect("add subcommand missing");
        let source_cmd = cmd
            .get_subcommands()
            .find(|sc| sc.get_name() == "source")
            .expect("source subcommand missing");
        let source_add_cmd = source_cmd
            .get_subcommands()
            .find(|sc| sc.get_name() == "add")
            .expect("source add subcommand missing");

        fn arg_shapes(
            cmd: &clap::Command,
        ) -> BTreeMap<String, (bool, Option<clap::builder::ValueRange>)> {
            cmd.get_arguments()
                .map(|a| {
                    (
                        a.get_id().as_str().to_string(),
                        (a.is_required_set(), a.get_num_args()),
                    )
                })
                .collect()
        }

        let add_args = arg_shapes(add_cmd);
        let source_add_args = arg_shapes(source_add_cmd);

        for flag in &[
            "sources",
            "refresh",
            "kind",
            "max_entries",
            "no_fetch_full_content",
        ] {
            let add_shape = add_args
                .get(*flag)
                .unwrap_or_else(|| panic!("`add` is missing --{flag}"));
            let source_add_shape = source_add_args
                .get(*flag)
                .unwrap_or_else(|| panic!("`source add` is missing --{flag}"));
            assert_eq!(
                add_shape, source_add_shape,
                "`--{flag}` differs between `add` and `source add`: {add_shape:?} vs {source_add_shape:?}"
            );
        }
    }

    /// `-s` short flag populates `stores`.
    #[test]
    fn short_store_flag() {
        let cli = Cli::try_parse_from(["localdb", "-s", "notes", "search", "foo"]).unwrap();
        assert_eq!(cli.stores, vec!["notes"]);
    }

    /// `localdb index --dir` is rejected by clap (flag was removed; use `--source` instead).
    #[test]
    fn index_dir_arg_is_rejected_by_clap() {
        let result = Cli::try_parse_from(["localdb", "index", "--dir", "/tmp/foo"]);
        assert!(
            result.is_err(),
            "expected --dir to be rejected, but clap accepted it"
        );
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
