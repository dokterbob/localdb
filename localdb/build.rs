//! Build-time version stamping (vergen-gitcl).
//!
//! Emits `VERGEN_GIT_SHA` (short), `VERGEN_GIT_DIRTY` and
//! `VERGEN_BUILD_TIMESTAMP` for `--version`. Errors never fail the build:
//! outside a git checkout (source tarball) vergen emits its idempotent
//! placeholder, which `main.rs` renders as `unknown`.

use vergen_gitcl::{Build, Emitter, Gitcl};

fn main() {
    // CI build number (GitHub Actions). Forwarded so the binary can report
    // exactly which pipeline run produced it.
    println!("cargo:rerun-if-env-changed=GITHUB_RUN_NUMBER");
    if let Ok(run) = std::env::var("GITHUB_RUN_NUMBER") {
        if !run.is_empty() {
            println!("cargo:rustc-env=LOCALDB_CI_RUN_NUMBER={run}");
        }
    }

    let build = Build::builder().build_timestamp(true).build();
    let gitcl = Gitcl::builder()
        .sha(true) // short SHA
        .dirty(false) // dirty flag; ignore untracked files
        .build();

    // Default Emitter does not fail on error: git problems degrade to the
    // idempotent placeholder value instead of breaking the build.
    Emitter::default()
        .add_instructions(&build)
        .expect("vergen build instructions must register")
        .add_instructions(&gitcl)
        .expect("vergen git instructions must register")
        .emit()
        .expect("vergen emit must succeed (fail_on_error is off)");
}
