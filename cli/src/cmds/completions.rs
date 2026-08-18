//! `localdb completions <shell>` — shell completion script generation.

pub use clap_complete::Shell;

/// `localdb completions <shell>`
///
/// Generates the completion script for `shell` on stdout, exit 0.
///
/// Pure codegen, like `internal print-schema`: no config load, no daemon
/// probe, no `command_table::dispatch`. The caller (`localdb/src/main.rs`)
/// passes its own top-level `clap::Command` so the script always matches the
/// binary's real CLI surface. Also the Homebrew formula's
/// `generate_completions_from_executable` entry point.
pub fn run_completions(shell: Shell, cmd: &mut clap::Command) {
    clap_complete::generate(shell, cmd, "localdb", &mut std::io::stdout());
}
