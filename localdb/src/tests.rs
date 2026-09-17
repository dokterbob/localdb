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
        "init", "serve", "mcp", "status", "store", "source", "document", "db", "job", "index",
        "search", "add",
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

/// Verify the document subcommands are present.
#[test]
fn document_subcommands_present() {
    use clap::CommandFactory;
    let cmd = Cli::command();
    let document_cmd = cmd
        .get_subcommands()
        .find(|sc| sc.get_name() == "document")
        .expect("document subcommand missing");

    let sub_names: Vec<&str> = document_cmd
        .get_subcommands()
        .map(|sc| sc.get_name())
        .collect();

    for expected in &["list", "get"] {
        assert!(
            sub_names.contains(expected),
            "document {expected} subcommand missing; found: {sub_names:?}",
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

/// Verify the job subcommands are present.
#[test]
fn job_subcommands_present() {
    use clap::CommandFactory;
    let cmd = Cli::command();
    let job_cmd = cmd
        .get_subcommands()
        .find(|sc| sc.get_name() == "job")
        .expect("job subcommand missing");

    let sub_names: Vec<&str> = job_cmd.get_subcommands().map(|sc| sc.get_name()).collect();

    for expected in &["cancel", "list"] {
        assert!(
            sub_names.contains(expected),
            "job {expected} subcommand missing; found: {sub_names:?}",
        );
    }
}

/// `localdb job list` parses with no arguments.
#[test]
fn job_list_parses() {
    let cli = Cli::try_parse_from(["localdb", "job", "list"]).unwrap();
    assert!(matches!(cli.command, Command::Job(JobCommand::List)));
}

/// `localdb job cancel <id>` parses the job id as a positional arg.
#[test]
fn job_cancel_parses() {
    let cli =
        Cli::try_parse_from(["localdb", "job", "cancel", "01HRQHB7FN3WMX4AZDV3S9VCTZ"]).unwrap();
    if let Command::Job(JobCommand::Cancel { id }) = cli.command {
        assert_eq!(id, "01HRQHB7FN3WMX4AZDV3S9VCTZ");
    } else {
        panic!("expected Job(Cancel) command");
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

/// `localdb init`/`localdb init --download-model` parse the new flag.
#[test]
fn init_download_model_flag_parses() {
    assert!(matches!(
        Cli::try_parse_from(["localdb", "init"]).unwrap().command,
        Command::Init {
            download_model: false
        }
    ));

    assert!(matches!(
        Cli::try_parse_from(["localdb", "init", "--download-model"])
            .unwrap()
            .command,
        Command::Init {
            download_model: true
        }
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
fn search_query_accepts_unquoted_multiple_words() {
    let cli = Cli::try_parse_from(["localdb", "search", "machine", "learning"]).unwrap();
    if let Command::Search {
        query,
        limit,
        content_length,
        ..
    } = cli.command
    {
        assert_eq!(query.join(" "), "machine learning");
        assert_eq!(limit, 3);
        assert_eq!(content_length, 1000);
    } else {
        panic!("expected Search command");
    }
}

/// A flag typed *after* the query words must still be parsed as a flag,
/// not silently absorbed into the query (issue #224). Before the fix,
/// `trailing_var_arg = true` made `--limit 5` here part of the query
/// text instead of setting `limit`.
#[test]
fn search_flags_after_query_words_still_parse() {
    let cli = Cli::try_parse_from(["localdb", "search", "rank", "fusion", "--limit", "5"]).unwrap();
    if let Command::Search { query, limit, .. } = cli.command {
        assert_eq!(query.join(" "), "rank fusion");
        assert_eq!(limit, 5);
    } else {
        panic!("expected Search command");
    }
}

/// `--limit` before the query words still works (regression guard).
#[test]
fn search_flags_before_query_words_still_parse() {
    let cli = Cli::try_parse_from(["localdb", "search", "--limit", "5", "rank", "fusion"]).unwrap();
    if let Command::Search { query, limit, .. } = cli.command {
        assert_eq!(query.join(" "), "rank fusion");
        assert_eq!(limit, 5);
    } else {
        panic!("expected Search command");
    }
}

/// `--` forces everything after it to be literal query text, including
/// tokens that look like flags — the escape hatch for a query word that
/// legitimately starts with `-`.
#[test]
fn search_double_dash_escapes_flag_like_query_words() {
    let cli = Cli::try_parse_from(["localdb", "search", "--", "--limit", "5"]).unwrap();
    if let Command::Search { query, limit, .. } = cli.command {
        assert_eq!(query, vec!["--limit".to_string(), "5".to_string()]);
        assert_eq!(limit, 3);
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

/// `localdb index --refetch` parses and sets the flag.
#[test]
fn index_refetch_flag_parses() {
    let cli = Cli::try_parse_from(["localdb", "index", "--refetch"]).unwrap();
    match cli.command {
        Command::Index { refetch, .. } => assert!(refetch),
        other => panic!("expected Index command, got: {other:?}"),
    }
}

/// `--refetch` is absent by default.
#[test]
fn index_without_refetch_flag_defaults_to_false() {
    let cli = Cli::try_parse_from(["localdb", "index"]).unwrap();
    match cli.command {
        Command::Index { refetch, .. } => assert!(!refetch),
        other => panic!("expected Index command, got: {other:?}"),
    }
}

/// `--refetch` combines with `--delete` and the global `-s` store filter
/// without conflict.
#[test]
fn index_refetch_combined_with_delete_and_store_flag_parses() {
    let cli =
        Cli::try_parse_from(["localdb", "-s", "notes", "index", "--refetch", "--delete"]).unwrap();
    assert_eq!(cli.stores, vec!["notes"]);
    match cli.command {
        Command::Index {
            refetch, delete, ..
        } => {
            assert!(refetch);
            assert!(delete);
        }
        other => panic!("expected Index command, got: {other:?}"),
    }
}

/// `-s` short flag works as a subcommand-level option too.
#[test]
fn short_store_flag_after_subcommand() {
    let cli =
        Cli::try_parse_from(["localdb", "search", "-s", "notes", "neural", "networks"]).unwrap();
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

#[test]
fn search_dedup_cli_default_and_modes() {
    for mode in ["off", "text", "text_and_vector"] {
        let parsed = Cli::try_parse_from(["localdb", "search", "query", "--dedup", mode]).unwrap();
        let Command::Search { dedup, .. } = parsed.command else {
            panic!("search expected")
        };
        assert_eq!(dedup.to_string(), mode);
    }
    let parsed = Cli::try_parse_from(["localdb", "search", "query"]).unwrap();
    let Command::Search { dedup, .. } = parsed.command else {
        panic!("search expected")
    };
    assert_eq!(dedup.to_string(), "text_and_vector");
    assert!(Cli::try_parse_from(["localdb", "search", "query", "--dedup", "approximate"]).is_err());
}
