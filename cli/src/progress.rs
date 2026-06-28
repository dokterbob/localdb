use std::io::IsTerminal as _;
use std::sync::{Arc, Mutex};

use indicatif::{ProgressBar, ProgressStyle};
use localdb_core::progress::{DocOutcome, ProgressEvent, ProgressSink};

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
    let pb: Arc<Mutex<Option<ProgressBar>>> = Arc::new(Mutex::new(None));

    // Chunk count accumulator shown in the message slot.
    let chunks: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));

    Arc::new(move |event: ProgressEvent| match event {
        ProgressEvent::SourceStarted { location, .. } => {
            let spinner = ProgressBar::new_spinner();
            spinner.set_style(
                ProgressStyle::with_template("{spinner} {msg}")
                    .unwrap()
                    .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
            );
            spinner.set_message(format!("Indexing {location}…"));
            spinner.enable_steady_tick(std::time::Duration::from_millis(80));
            *pb.lock().unwrap() = Some(spinner);
            *chunks.lock().unwrap() = 0;
        }
        ProgressEvent::Discovered { total } => {
            let mut guard = pb.lock().unwrap();
            if let Some(old) = guard.take() {
                old.finish_and_clear();
            }
            let bar = ProgressBar::new(total as u64);
            bar.set_style(
                ProgressStyle::with_template(
                    "{spinner} [{wide_bar}] {pos}/{len} (eta {eta}) {msg}",
                )
                .unwrap()
                .progress_chars("=>-"),
            );
            bar.enable_steady_tick(std::time::Duration::from_millis(80));
            *guard = Some(bar);
        }
        ProgressEvent::DocumentStarted { uri, .. } => {
            let guard = pb.lock().unwrap();
            if let Some(bar) = guard.as_ref() {
                let name = uri.rsplit('/').next().unwrap_or(&uri).to_string();
                bar.set_message(name);
            }
        }
        ProgressEvent::DocumentFinished { outcome, .. } => {
            let guard = pb.lock().unwrap();
            if let Some(bar) = guard.as_ref() {
                if let DocOutcome::Indexed { chunks: c } = outcome {
                    let mut total_chunks = chunks.lock().unwrap();
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
                "  indexed {} docs, {} skipped, {} chunks",
                result.docs_indexed, result.docs_skipped, result.chunks_written
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
    let state: Arc<Mutex<PlainState>> = Arc::new(Mutex::new(PlainState::new()));

    Arc::new(move |event: ProgressEvent| {
        let mut s = state.lock().unwrap();
        match event {
            ProgressEvent::SourceStarted { location, .. } => {
                *s = PlainState::new();
                eprintln!("Indexing {location}");
            }
            ProgressEvent::Discovered { total } => {
                s.total = total;
                eprintln!("  discovered {} files", total);
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
                    eprintln!("  {}", format_plain_progress(s.done, s.total, s.chunks));
                    s.last_reported_done = s.done;
                }
            }
            ProgressEvent::SourceFinished { result } => {
                eprintln!(
                    "  indexed {} docs, {} skipped, {} chunks",
                    result.docs_indexed, result.docs_skipped, result.chunks_written
                );
            }
        }
    })
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
        let sink = plain_sink();
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
    }
}
