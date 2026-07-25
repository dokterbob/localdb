use std::io::IsTerminal as _;
use std::sync::{Arc, Mutex};

use indicatif::{ProgressBar, ProgressStyle};
use localdb_core::progress::{DocOutcome, ProgressEvent, ProgressSink};
use localdb_core::uri::display_decoded_uri;
use store_libsql::{MigrationProgressEvent, MigrationProgressSink};

fn lock_or_poison<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn spinner_style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template).unwrap_or_else(|_| ProgressStyle::default_spinner())
}

fn bar_style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template).unwrap_or_else(|_| ProgressStyle::default_bar())
}

/// Build a progress sink for CLI use.
///
/// Returns `None` when `--json` is active (stdout must be clean).
/// Returns `Some(sink)` otherwise; the sink drives an animated bar on a TTY
/// or periodic plain `eprintln!` lines when stderr is piped.
pub fn build_progress_sink(json_mode: bool) -> Option<ProgressSink> {
    if json_mode {
        return None;
    }

    if std::io::stderr().is_terminal() {
        Some(tty_sink())
    } else {
        Some(plain_sink())
    }
}

// ---------------------------------------------------------------------------
// TTY renderer — indicatif bar
// ---------------------------------------------------------------------------

fn tty_sink() -> ProgressSink {
    let (sink, _, _) = tty_sink_parts();
    sink
}

type TtySinkParts = (
    ProgressSink,
    Arc<Mutex<Option<ProgressBar>>>,
    Arc<Mutex<usize>>,
);

fn tty_sink_parts() -> TtySinkParts {
    let pb: Arc<Mutex<Option<ProgressBar>>> = Arc::new(Mutex::new(None));

    // Chunk count accumulator shown in the message slot.
    let chunks: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));

    let pb_for_sink = Arc::clone(&pb);
    let chunks_for_sink = Arc::clone(&chunks);

    let sink = Arc::new(move |event: ProgressEvent| match event {
        ProgressEvent::SourceStarted { location, .. } => {
            let spinner = ProgressBar::new_spinner();
            spinner.set_style(
                spinner_style("{spinner} {msg}")
                    .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
            );
            spinner.set_message(format!("Indexing {location}…"));
            spinner.enable_steady_tick(std::time::Duration::from_millis(80));
            *lock_or_poison(&pb_for_sink) = Some(spinner);
            *lock_or_poison(&chunks_for_sink) = 0;
        }
        ProgressEvent::Discovered { total } => {
            let mut guard = lock_or_poison(&pb_for_sink);
            if let Some(old) = guard.take() {
                old.finish_and_clear();
            }
            let bar = ProgressBar::new(total as u64);
            bar.set_style(
                bar_style("{spinner} [{wide_bar}] {pos}/{len} (eta {eta}) {msg}")
                    .progress_chars("=>-"),
            );
            bar.enable_steady_tick(std::time::Duration::from_millis(80));
            *guard = Some(bar);
        }
        ProgressEvent::DocumentStarted { uri, .. } => {
            let guard = lock_or_poison(&pb_for_sink);
            if let Some(bar) = guard.as_ref() {
                let decoded = display_decoded_uri(&uri);
                let name = decoded.rsplit('/').next().unwrap_or(&decoded).to_string();
                bar.set_message(name);
            }
        }
        ProgressEvent::DocumentFinished { outcome, .. } => {
            let guard = lock_or_poison(&pb_for_sink);
            if let Some(bar) = guard.as_ref() {
                if let DocOutcome::Indexed { chunks: c } = outcome {
                    let mut total_chunks = lock_or_poison(&chunks_for_sink);
                    *total_chunks += c;
                    bar.set_message(format!("{} chunks", *total_chunks));
                }
                bar.inc(1);
            }
        }
        ProgressEvent::SourceFinished { result } => {
            let mut guard = lock_or_poison(&pb_for_sink);
            if let Some(bar) = guard.take() {
                bar.finish_and_clear();
            }
            eprintln!(
                "  indexed {} docs, {} skipped, {} deleted, {} chunks",
                result.docs_indexed,
                result.docs_skipped,
                result.docs_deleted,
                result.chunks_written
            );
        }
    });

    (sink, pb, chunks)
}

// ---------------------------------------------------------------------------
// Plain (pipe / CI) renderer — bounded eprintln! lines
// ---------------------------------------------------------------------------

/// State shared across plain-mode sink invocations.
struct PlainState {
    total: usize,
    done: usize,
    chunks: usize,
    last_reported_done: usize,
}

impl PlainState {
    fn new() -> Self {
        Self {
            total: 0,
            done: 0,
            chunks: 0,
            last_reported_done: 0,
        }
    }
}

/// How often to emit a mid-progress line in plain mode.
const PLAIN_REPORT_INTERVAL: usize = 10;

fn plain_sink() -> ProgressSink {
    plain_sink_with_emitter(Arc::new(|line: String| {
        eprintln!("{line}");
    }))
}

fn plain_sink_with_emitter(writer: Arc<dyn Fn(String) + Send + Sync>) -> ProgressSink {
    let state: Arc<Mutex<PlainState>> = Arc::new(Mutex::new(PlainState::new()));

    Arc::new(move |event: ProgressEvent| {
        let mut s = lock_or_poison(&state);
        match event {
            ProgressEvent::SourceStarted { location, .. } => {
                *s = PlainState::new();
                writer(format!("Indexing {location}"));
            }
            ProgressEvent::Discovered { total } => {
                s.total = total;
                writer(format!("  discovered {} files", total));
            }
            ProgressEvent::DocumentStarted { .. } => {}
            ProgressEvent::DocumentFinished { outcome, .. } => {
                s.done += 1;
                if let DocOutcome::Indexed { chunks } = outcome {
                    s.chunks += chunks;
                }
                let interval = if s.total > 0 {
                    (s.total / 10).max(PLAIN_REPORT_INTERVAL)
                } else {
                    PLAIN_REPORT_INTERVAL
                };
                if s.done - s.last_reported_done >= interval {
                    writer(format!(
                        "  {}",
                        format_plain_progress(s.done, s.total, s.chunks)
                    ));
                    s.last_reported_done = s.done;
                }
            }
            ProgressEvent::SourceFinished { result } => {
                writer(format!(
                    "  indexed {} docs, {} skipped, {} deleted, {} chunks",
                    result.docs_indexed,
                    result.docs_skipped,
                    result.docs_deleted,
                    result.chunks_written
                ));
            }
        }
    })
}

#[cfg(test)]
fn plain_sink_with_writer(writer: Arc<Mutex<Vec<String>>>) -> ProgressSink {
    plain_sink_with_emitter(Arc::new(move |line: String| {
        lock_or_poison(&writer).push(line);
    }))
}

/// Pure function: format a mid-progress status line. Unit-testable.
pub fn format_plain_progress(done: usize, total: usize, chunks: usize) -> String {
    if total > 0 {
        format!("indexed {}/{} ({} chunks)", done, total, chunks)
    } else {
        format!("indexed {} ({} chunks)", done, chunks)
    }
}

// ---------------------------------------------------------------------------
// Migration progress (`db migrate`) — PR #152 comment: minutes of disk I/O
// with zero feedback. Mirrors `build_progress_sink`'s TTY/JSON/pipe rules,
// but against `store_libsql::MigrationProgressEvent` — a different event
// vocabulary from reindex's `ProgressEvent`, since migration steps have no
// per-document/chunk shape to report.
// ---------------------------------------------------------------------------

/// Build a progress sink for `localdb db migrate`.
///
/// Returns `None` when `--json` is active (stdout must stay clean JSON).
/// Returns `Some(sink)` otherwise: an animated spinner on a TTY (a heartbeat
/// via `enable_steady_tick`, so it keeps animating even during a single
/// long-running step with no intervening events) or bounded plain
/// `eprintln!` lines (one per step) when stderr is piped.
pub fn build_migration_progress_sink(json_mode: bool) -> Option<MigrationProgressSink> {
    if json_mode {
        return None;
    }

    if std::io::stderr().is_terminal() {
        Some(migration_tty_sink())
    } else {
        Some(migration_plain_sink())
    }
}

/// Pure function: format the `db migrate` spinner/plain "applying" message.
/// Unit-testable.
pub fn format_applying_step(index: usize, total: usize, name: &str) -> String {
    format!("applying {index}/{total}: {name}")
}

fn new_migration_spinner() -> ProgressBar {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        spinner_style("{spinner} {msg}")
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    spinner.enable_steady_tick(std::time::Duration::from_millis(80));
    spinner
}

fn migration_tty_sink() -> MigrationProgressSink {
    let pb: Arc<Mutex<Option<ProgressBar>>> = Arc::new(Mutex::new(None));

    Arc::new(move |event: MigrationProgressEvent| {
        let mut guard = lock_or_poison(&pb);
        match event {
            MigrationProgressEvent::Started { total_pending } => {
                let spinner = new_migration_spinner();
                spinner.set_message(if total_pending > 0 {
                    format!(
                        "applying {total_pending} pending migration{s}…",
                        s = if total_pending == 1 { "" } else { "s" }
                    )
                } else {
                    "checking schema…".to_string()
                });
                *guard = Some(spinner);
            }
            MigrationProgressEvent::Initializing => {
                let spinner = new_migration_spinner();
                spinner.set_message("initializing store…");
                *guard = Some(spinner);
            }
            MigrationProgressEvent::RebuildingLegacy => {
                let spinner = new_migration_spinner();
                spinner.set_message("rebuilding legacy store…");
                *guard = Some(spinner);
            }
            MigrationProgressEvent::ApplyingStep {
                index, total, name, ..
            } => {
                if let Some(bar) = guard.as_ref() {
                    bar.set_message(format_applying_step(index, total, &name));
                }
            }
            MigrationProgressEvent::Finished => {
                if let Some(bar) = guard.take() {
                    bar.finish_and_clear();
                }
            }
        }
    })
}

fn migration_plain_sink() -> MigrationProgressSink {
    migration_plain_sink_with_emitter(Arc::new(|line: String| {
        eprintln!("{line}");
    }))
}

fn migration_plain_sink_with_emitter(
    writer: Arc<dyn Fn(String) + Send + Sync>,
) -> MigrationProgressSink {
    Arc::new(move |event: MigrationProgressEvent| match event {
        MigrationProgressEvent::Started { total_pending } => {
            if total_pending > 0 {
                writer(format!(
                    "applying {total_pending} pending migration{s}",
                    s = if total_pending == 1 { "" } else { "s" }
                ));
            }
        }
        MigrationProgressEvent::Initializing => writer("initializing store".to_string()),
        MigrationProgressEvent::RebuildingLegacy => writer("rebuilding legacy store".to_string()),
        MigrationProgressEvent::ApplyingStep {
            index, total, name, ..
        } => {
            writer(format_applying_step(index, total, &name));
        }
        MigrationProgressEvent::Finished => {}
    })
}

#[cfg(test)]
fn migration_plain_sink_with_writer(writer: Arc<Mutex<Vec<String>>>) -> MigrationProgressSink {
    migration_plain_sink_with_emitter(Arc::new(move |line: String| {
        lock_or_poison(&writer).push(line);
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_plain_progress_with_total() {
        let s = format_plain_progress(3, 10, 42);
        assert_eq!(s, "indexed 3/10 (42 chunks)");
    }

    #[test]
    fn format_plain_progress_no_total() {
        let s = format_plain_progress(5, 0, 7);
        assert_eq!(s, "indexed 5 (7 chunks)");
    }

    #[test]
    fn format_plain_progress_zero() {
        let s = format_plain_progress(0, 0, 0);
        assert_eq!(s, "indexed 0 (0 chunks)");
    }

    #[test]
    fn build_progress_sink_json_returns_none() {
        let sink = build_progress_sink(true);
        assert!(sink.is_none());
    }

    #[test]
    fn plain_sink_does_not_panic_on_full_sequence() {
        // Simulate a non-TTY sink driving through a full event sequence.
        let writer = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = plain_sink_with_writer(Arc::clone(&writer));
        sink(ProgressEvent::SourceStarted {
            source_id: "s1".to_string(),
            location: "/tmp/test".to_string(),
        });
        sink(ProgressEvent::Discovered { total: 3 });
        for i in 0..3usize {
            let uri = format!("file:///tmp/test/doc{}.md", i);
            sink(ProgressEvent::DocumentStarted {
                uri: uri.clone(),
                index: i,
                total: 3,
            });
            sink(ProgressEvent::DocumentFinished {
                uri,
                outcome: DocOutcome::Indexed { chunks: 2 },
            });
        }
        sink(ProgressEvent::SourceFinished {
            result: localdb_core::ingestion::IngestionResult {
                docs_seen: 3,
                docs_indexed: 3,
                docs_skipped: 0,
                docs_deleted: 0,
                chunks_written: 6,
                unsupported_format_count: 0,
                error_count: 0,
            },
        });

        let output = lock_or_poison(&writer).clone();
        assert!(output
            .iter()
            .any(|line| line.contains("Indexing /tmp/test")));
        assert!(output
            .iter()
            .any(|line| line.contains("chunks_written: 6") || line.contains("6 chunk")));
        assert_eq!(
            output,
            vec![
                "Indexing /tmp/test".to_string(),
                "  discovered 3 files".to_string(),
                "  indexed 3 docs, 0 skipped, 0 deleted, 6 chunks".to_string(),
            ]
        );
    }

    // Part B.2 (PR #152 comment): the delete-sweep's count was previously
    // invisible in the SourceFinished summary line — a source silently
    // deleting thousands of resources produced no visible signal. Assert the
    // deleted count now appears.
    #[test]
    fn plain_sink_reports_nonzero_deleted_count() {
        let writer = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = plain_sink_with_writer(Arc::clone(&writer));
        sink(ProgressEvent::SourceStarted {
            source_id: "s1".to_string(),
            location: "/tmp/test".to_string(),
        });
        sink(ProgressEvent::SourceFinished {
            result: localdb_core::ingestion::IngestionResult {
                docs_seen: 10,
                docs_indexed: 6,
                docs_skipped: 0,
                docs_deleted: 4394,
                chunks_written: 12,
                unsupported_format_count: 0,
                error_count: 0,
            },
        });

        let output = lock_or_poison(&writer).clone();
        assert!(
            output.iter().any(|line| line.contains("4394 deleted")),
            "deleted count must be visible in the summary line: {output:?}"
        );
    }

    #[test]
    fn poisoned_lock_does_not_panic() {
        use std::panic::{catch_unwind, AssertUnwindSafe};
        use std::thread;

        let (sink, pb, _) = tty_sink_parts();
        let poison = Arc::clone(&pb);

        let handle = thread::spawn(move || {
            let _guard = lock_or_poison(&poison);
            panic!("poison progress mutex");
        });

        let _ = handle.join();

        let result = catch_unwind(AssertUnwindSafe(|| {
            sink(ProgressEvent::DocumentStarted {
                uri: "file:///tmp/test/doc.md".to_string(),
                index: 0,
                total: 1,
            });
        }));

        assert!(result.is_ok());
    }

    // Part B: `DocumentStarted` carries the raw `Uri::as_str()` string
    // (percent-encoded), but the progress bar must show a human-readable
    // name — decode before extracting the trailing path segment, don't
    // display "my%20file.md" for a file literally named "my file.md".
    #[test]
    fn tty_sink_decodes_percent_encoded_uri_for_display() {
        let (sink, pb, _) = tty_sink_parts();
        sink(ProgressEvent::SourceStarted {
            source_id: "s1".to_string(),
            location: "/tmp/test".to_string(),
        });
        sink(ProgressEvent::Discovered { total: 1 });
        sink(ProgressEvent::DocumentStarted {
            uri: "file:///tmp/test/my%20file.md".to_string(),
            index: 0,
            total: 1,
        });

        let guard = lock_or_poison(&pb);
        let bar = guard.as_ref().expect("bar should exist after Discovered");
        assert_eq!(bar.message(), "my file.md");
    }

    // -- `db migrate` progress rendering (Part B.1) -------------------------

    #[test]
    fn format_applying_step_renders_one_based_index_and_name() {
        assert_eq!(
            format_applying_step(2, 5, "add_widgets"),
            "applying 2/5: add_widgets"
        );
    }

    #[test]
    fn build_migration_progress_sink_json_returns_none() {
        assert!(build_migration_progress_sink(true).is_none());
    }

    #[test]
    fn migration_plain_sink_reports_started_step_and_ignores_finished() {
        let writer = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = migration_plain_sink_with_writer(Arc::clone(&writer));

        sink(MigrationProgressEvent::Started { total_pending: 2 });
        sink(MigrationProgressEvent::ApplyingStep {
            index: 1,
            total: 2,
            version: 5,
            name: "add_widgets".to_string(),
        });
        sink(MigrationProgressEvent::ApplyingStep {
            index: 2,
            total: 2,
            version: 6,
            name: "add_gadgets".to_string(),
        });
        sink(MigrationProgressEvent::Finished);

        let output = lock_or_poison(&writer).clone();
        assert_eq!(
            output,
            vec![
                "applying 2 pending migrations".to_string(),
                "applying 1/2: add_widgets".to_string(),
                "applying 2/2: add_gadgets".to_string(),
            ],
            "Finished must emit nothing to stderr in plain mode: {output:?}"
        );
    }

    #[test]
    fn migration_plain_sink_started_with_zero_pending_emits_nothing() {
        // A no-op-at-head `db migrate` call still emits `Started {
        // total_pending: 0 }` (verification still ran) — plain mode must not
        // print a misleading "applying 0 pending migrations" line for it.
        let writer = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = migration_plain_sink_with_writer(Arc::clone(&writer));

        sink(MigrationProgressEvent::Started { total_pending: 0 });
        sink(MigrationProgressEvent::Finished);

        let output = lock_or_poison(&writer).clone();
        assert!(output.is_empty(), "expected no output, got: {output:?}");
    }

    #[test]
    fn migration_plain_sink_reports_initializing_and_rebuilding_signals() {
        let writer = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = migration_plain_sink_with_writer(Arc::clone(&writer));
        sink(MigrationProgressEvent::Initializing);
        assert_eq!(
            lock_or_poison(&writer).clone(),
            vec!["initializing store".to_string()]
        );

        let writer2 = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink2 = migration_plain_sink_with_writer(Arc::clone(&writer2));
        sink2(MigrationProgressEvent::RebuildingLegacy);
        assert_eq!(
            lock_or_poison(&writer2).clone(),
            vec!["rebuilding legacy store".to_string()]
        );
    }
}
